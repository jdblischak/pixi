use std::io;
use std::io::{stdout, Write};
use std::path::PathBuf;

use clap::Parser;
use console::Color;
use human_bytes::human_bytes;
use itertools::Itertools;
use rattler_conda_types::Platform;
use rattler_lock::Package;
use serde::Serialize;
use uv_distribution::RegistryWheelIndex;

use crate::lock_file::{UpdateLockFileOptions, UvResolutionContext};
use crate::project::manifest::EnvironmentName;
use crate::pypi_tags::{get_pypi_tags, is_python_record};
use crate::Project;

use crate::consts::PROJECT_MANIFEST;

// an enum to sort by size or name
#[derive(clap::ValueEnum, Clone, Debug, Serialize)]
pub enum SortBy {
    Size,
    Name,
    Type,
}

/// List project's packages. Highlighted packages are explicit dependencies.
#[derive(Debug, Parser)]
#[clap(arg_required_else_help = false)]
pub struct Args {
    /// List only packages matching a regular expression
    #[arg()]
    pub regex: Option<String>,

    /// The platform to list packages for. Defaults to the current platform.
    #[arg(long)]
    pub platform: Option<Platform>,

    /// Whether to output in json format
    #[arg(long)]
    pub json: bool,

    /// Whether to output in pretty json format
    #[arg(long)]
    pub json_pretty: bool,

    /// Sorting strategy
    #[arg(long, default_value = "name", value_enum)]
    pub sort_by: SortBy,

    /// The path to 'pixi.toml'
    #[arg(long)]
    pub manifest_path: Option<PathBuf>,

    /// The environment to list packages for. Defaults to the default environment.
    #[arg(short, long)]
    pub environment: Option<String>,

    #[clap(flatten)]
    pub lock_file_usage: super::LockFileUsageArgs,

    /// Don't install the environment for pypi solving, only update the lock-file if it can solve without installing.
    #[arg(long)]
    pub no_install: bool,
}

#[derive(Serialize)]
struct PackageToOutput {
    name: String,
    version: String,
    build: Option<String>,
    size_bytes: Option<u64>,
    kind: String,
    source: Option<String>,
    is_explicit: bool,
}

/// Get directory size
pub fn get_dir_size<P>(path: P) -> std::io::Result<u64>
where
    P: AsRef<std::path::Path>,
{
    let mut result = 0;

    if path.as_ref().is_dir() {
        for entry in std::fs::read_dir(&path)? {
            let _path = entry?.path();
            if _path.is_file() {
                result += _path.metadata()?.len();
            } else {
                result += get_dir_size(_path)?;
            }
        }
    } else {
        result = path.as_ref().metadata()?.len();
    }
    Ok(result)
}

pub async fn execute(args: Args) -> miette::Result<()> {
    let project = Project::load_or_else_discover(args.manifest_path.as_deref())?;
    let environment_name = args
        .environment
        .map_or_else(|| EnvironmentName::Default, EnvironmentName::Named);
    let environment = project
        .environment(&environment_name)
        .ok_or_else(|| miette::miette!("unknown environment '{environment_name}'"))?;

    let lock_file = project
        .up_to_date_lock_file(UpdateLockFileOptions {
            lock_file_usage: args.lock_file_usage.into(),
            no_install: args.no_install,
            ..UpdateLockFileOptions::default()
        })
        .await?;

    // Load the platform
    let platform = args.platform.unwrap_or_else(Platform::current);

    // Get all the packages in the environment.
    let locked_deps = lock_file
        .lock_file
        .environment(environment.name().as_str())
        .and_then(|env| env.packages(platform).map(Vec::from_iter))
        .unwrap_or_default();

    // Get the uv structs for extra pypi info
    let uv_context = UvResolutionContext::from_project(&project)?;
    // Get the python record from the lock file
    let mut conda_records = locked_deps.iter().filter_map(|d| d.as_conda());
    // Determine the current environment markers.
    let python_record = conda_records
        .find(|r| is_python_record(r))
        .ok_or_else(|| miette::miette!("could not resolve pypi dependencies because no python interpreter is added to the dependencies of the project.\nMake sure to add a python interpreter to the [dependencies] section of the {PROJECT_MANIFEST}, or run:\n\n\tpixi add python"))?;
    let tags = get_pypi_tags(
        Platform::current(),
        &project.system_requirements(),
        python_record.package_record(),
    )?;
    let mut registry_index =
        RegistryWheelIndex::new(&uv_context.cache, &tags, &uv_context.index_locations);

    // Get the explicit project dependencies
    let mut project_dependency_names = environment
        .dependencies(None, Some(platform))
        .names()
        .map(|p| p.as_source().to_string())
        .collect_vec();
    project_dependency_names.extend(
        environment
            .pypi_dependencies(Some(platform))
            .into_iter()
            .map(|(name, _)| name.as_source().to_string()),
    );
    // Convert the list of package record to specific output format
    let mut packages_to_output = locked_deps
        .iter()
        .map(|p| create_package_to_output(p, &project_dependency_names, &mut registry_index))
        .collect::<Vec<PackageToOutput>>();

    // Filter packages by regex if needed
    if let Some(regex) = args.regex {
        let regex = regex::Regex::new(&regex).map_err(|_| miette::miette!("Invalid regex"))?;
        packages_to_output = packages_to_output
            .into_iter()
            .filter(|p| regex.is_match(&p.name))
            .collect::<Vec<_>>();
    }

    // Sort according to the sorting strategy
    match args.sort_by {
        SortBy::Size => {
            packages_to_output
                .sort_by(|a, b| a.size_bytes.unwrap_or(0).cmp(&b.size_bytes.unwrap_or(0)));
        }
        SortBy::Name => {
            packages_to_output.sort_by(|a, b| a.name.cmp(&b.name));
        }
        SortBy::Type => {
            packages_to_output.sort_by(|a, b| a.kind.cmp(&b.kind));
        }
    }

    if packages_to_output.is_empty() {
        eprintln!(
            "{}No packages found.",
            console::style(console::Emoji("✘ ", "")).red(),
        );
        return Ok(());
    }

    // Print as table string or JSON
    if args.json || args.json_pretty {
        // print packages as json
        json_packages(&packages_to_output, args.json_pretty);
    } else {
        // print packages as table
        print_packages_as_table(&packages_to_output).expect("an io error occurred");
    }

    Ok(())
}

fn print_packages_as_table(packages: &Vec<PackageToOutput>) -> io::Result<()> {
    let mut writer = tabwriter::TabWriter::new(stdout());

    let header_style = console::Style::new().bold();
    writeln!(
        writer,
        "{}\t{}\t{}\t{}\t{}\t{}",
        header_style.apply_to("Package"),
        header_style.apply_to("Version"),
        header_style.apply_to("Build"),
        header_style.apply_to("Size"),
        header_style.apply_to("Kind"),
        header_style.apply_to("Source")
    )?;

    for package in packages {
        if package.is_explicit {
            write!(
                writer,
                "{}",
                console::style(&package.name).fg(Color::Green).bold()
            )?
        } else {
            write!(writer, "{}", &package.name)?;
        };

        // Convert size to human readable format
        let size_human = package
            .size_bytes
            .map(|size| human_bytes(size as f64))
            .unwrap_or_default();

        writeln!(
            writer,
            "\t{}\t{}\t{}\t{}\t{}",
            &package.version,
            package.build.as_deref().unwrap_or(""),
            size_human,
            &package.kind,
            package.source.as_deref().unwrap_or("")
        )?;
    }

    writer.flush()
}

fn json_packages(packages: &Vec<PackageToOutput>, json_pretty: bool) {
    let json_string = if json_pretty {
        serde_json::to_string_pretty(&packages)
    } else {
        serde_json::to_string(&packages)
    }
    .expect("Cannot serialize packages to JSON");

    println!("{}", json_string);
}

fn create_package_to_output<'a, 'b>(
    p: &'b Package,
    project_dependency_names: &'a [String],
    registry_index: &'a mut RegistryWheelIndex<'b>,
) -> PackageToOutput {
    let name = p.name().to_string();
    let version = p.version().into_owned();

    let kind = match p {
        Package::Conda(_) => "conda".to_string(),
        Package::Pypi(_) => "pypi".to_string(),
    };
    let build = match p {
        Package::Conda(pkg) => Some(pkg.package_record().build.clone()),
        Package::Pypi(_) => None,
    };

    let size_bytes = match p {
        Package::Conda(pkg) => pkg.package_record().size,
        Package::Pypi(p) => {
            let package_data = p.data().package;
            registry_index
                .get_version(&package_data.name, &package_data.version)
                .map(|c| c.path.clone())
                .and_then(|p| get_dir_size(p).ok())
        }
    };

    let source = match p {
        Package::Conda(pkg) => pkg.file_name().map(ToOwned::to_owned),
        Package::Pypi(p) => {
            let package_data = p.data().package;
            registry_index
                .get_version(&package_data.name, &package_data.version)
                .map(|c| c.filename.to_string())
        }
    };

    let is_explicit = project_dependency_names.contains(&name);

    PackageToOutput {
        name,
        version,
        build,
        size_bytes,
        kind,
        source,
        is_explicit,
    }
}
