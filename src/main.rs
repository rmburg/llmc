#![allow(unstable_name_collisions)]

use std::{
    collections::{BTreeSet, HashSet},
    env::{self, home_dir},
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use itertools::Itertools;
use serde::Deserialize;

const DEFAULT_IMAGE: &str = "localhost/llmc:latest";
const CONTAINERFILE_SOURCE: &[u8] = include_bytes!("../Containerfile");

const DEFAULT_PACKAGES: &[&str] = &["opencode", "git", "base-devel", "less", "sudo"];
const DEFAULT_MOUNTS: &[(&str, bool)] = &[
    ("~/.config/opencode", false),
    ("~/.local/share/opencode", false),
    ("~/.local/state/opencode", false),
    ("~/.cache/opencode", false),
];

#[derive(Debug, Parser)]
#[command(name = "llmc", about = "Sei kein Dummi, nimm ein Gummi!")]
struct Cli {
    #[arg(long, help = "Rebuild the llmc container image and exit")]
    build: bool,

    #[arg(long, help = "Launch a shell in the LLMC container")]
    shell: bool,

    #[arg(short, long, value_name = "FILE", help = "Path to llmc TOML config")]
    config: Option<PathBuf>,

    #[arg(
        value_name = "PATH",
        help = "Paths to bind-mount. The first path becomes the container workdir."
    )]
    paths: Vec<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ConfigFile {
    image: Option<String>,
    build_context: Option<PathBuf>,
    #[serde(default)]
    extra_packages: Vec<String>,
    #[serde(default)]
    mounts: Vec<Mount>,
}

#[derive(Debug)]
struct LoadedConfig {
    file: ConfigFile,
    base_dir: PathBuf,
}

#[derive(Debug)]
struct Config {
    image: String,
    packages: BTreeSet<String>,
    mounts: HashSet<Mount>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct Mount {
    path: PathBuf,
    target: Option<PathBuf>,
    #[serde(default)]
    readonly: bool,
}

impl Mount {
    fn new(path: impl Into<PathBuf>, readonly: bool) -> Self {
        let path = path.into();
        Self {
            path,
            target: None,
            readonly,
        }
    }

    fn spec(&self) -> String {
        let mut spec = format!(
            "{}:{}",
            self.path.display(),
            self.target.as_ref().unwrap_or(&self.path).display()
        );
        if self.readonly {
            spec.push_str(":ro");
        }
        spec
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cwd = env::current_dir().context("failed to get current directory")?;
    let home = home_dir().context("failed to find home dir")?;
    let loaded = load_config(cli.config.as_deref(), &cwd, &home)?;
    let config = Config::from_file(loaded.file, &loaded.base_dir, &home)?;

    if cli.build {
        if !cli.paths.is_empty() {
            bail!("path arguments are not used with --build");
        }

        return build_image(&config);
    }

    if cli.shell {
        return run_in_container(&cli, &config, &home, &["bash"]);
    }

    run_in_container(&cli, &config, &home, &["opencode", "--standalone"])
}

impl Config {
    fn from_file(file: ConfigFile, base_dir: &Path, home: &Path) -> Result<Self> {
        let image = file.image.unwrap_or_else(|| DEFAULT_IMAGE.to_string());
        if image.trim().is_empty() {
            bail!("image cannot be empty");
        }

        let packages = {
            let mut packages = BTreeSet::<String>::new();
            packages.extend(DEFAULT_PACKAGES.iter().copied().map(String::from));
            packages.extend(file.extra_packages);

            packages
        };

        let mounts = {
            let mut mounts = HashSet::<Mount>::new();
            mounts.extend(
                DEFAULT_MOUNTS
                    .iter()
                    .map(|(path, readonly)| -> Result<Mount> {
                        let path = resolve_config_source(Path::new(*path), base_dir, home)?;

                        Ok(Mount::new(path, *readonly))
                    })
                    .collect::<Result<Vec<Mount>>>()?,
            );
            mounts.extend(
                file.mounts
                    .into_iter()
                    .map(
                        |Mount {
                             path,
                             target,
                             readonly,
                         }| {
                            Ok(Mount {
                                path: resolve_config_source(&path, base_dir, home)?,
                                target,
                                readonly,
                            })
                        },
                    )
                    .collect::<Result<Vec<_>>>()?,
            );

            mounts
        };

        Ok(Self {
            image,
            packages,
            mounts,
        })
    }
}

fn check_if_podman_is_installed() -> Result<()> {
    Command::new("podman")
        .arg("--version")
        .status()
        .context("Podman is not installed")?;

    Ok(())
}

fn check_if_image_exists(image: &str) -> Result<()> {
    Command::new("podman")
        .arg("image")
        .arg("exists")
        .arg(image)
        .status()
        .context("Podman is not installed")?
        .success()
        .ok_or(anyhow!(
            "Podman LLMC image does not exist, please run llmc --build first"
        ))
}

fn run_in_container(
    cli: &Cli,
    config: &Config,
    home: &Path,
    command_and_args: &[&str],
) -> Result<()> {
    check_if_podman_is_installed()?;
    check_if_image_exists(&config.image)?;

    let mut mounts = config.mounts.clone();
    for path in &cli.paths {
        let source = path.canonicalize()?;
        mounts.insert(Mount {
            path: source,
            target: None,
            readonly: false,
        });
    }

    for mount in &mounts {
        if !std::fs::exists(&mount.path)? {
            bail!("Mount point does not exist: {}", mount.path.display());
        }
    }

    let workdir = cli
        .paths
        .first()
        .and_then(|path| path.canonicalize().ok())
        .unwrap_or_else(|| home.to_path_buf());

    let mut command = Command::new("podman");
    command
        .arg("run")
        .arg("-it")
        .arg("--rm")
        .arg("--init")
        .arg("--userns")
        .arg("keep-id");

    for mount in mounts {
        command.arg("-v").arg(mount.spec());
    }

    command
        .arg("-w")
        .arg(workdir)
        .arg(&config.image)
        .args(command_and_args);

    let status = command.status().context("failed to run podman")?;

    if !status.success() {
        bail!("podman run failed");
    }

    Ok(())
}

fn build_image(config: &Config) -> Result<()> {
    check_if_podman_is_installed()?;

    let mut containerfile = tempfile::Builder::new().tempfile()?;

    containerfile.write_all(CONTAINERFILE_SOURCE)?;

    let mut command = Command::new("podman");
    command
        .arg("build")
        .arg("--no-cache")
        .arg("--file")
        .arg(containerfile.path())
        .arg("--build-arg")
        .arg(format!("USERNAME={}", command_output("id", &["-un"])?))
        .arg("--build-arg")
        .arg(format!("UID={}", command_output("id", &["-u"])?))
        .arg("--build-arg")
        .arg(format!("GID={}", command_output("id", &["-g"])?))
        .arg("--build-arg")
        .arg(format!(
            "PACKAGES={}",
            config
                .packages
                .iter()
                .map(String::as_str)
                .intersperse(" ")
                .collect::<String>()
        ))
        .arg("-t")
        .arg(&config.image);

    let status = command.status().context("failed to run podman build")?;

    if !status.success() {
        bail!("podman build failed");
    }

    Ok(())
}

fn load_config(config_arg: Option<&Path>, cwd: &Path, home: &Path) -> Result<LoadedConfig> {
    let config_path = if let Some(path) = config_arg {
        Some(resolve_path(path, cwd)?)
    } else {
        let local = cwd.join("llmc.toml");
        let home_config = home.join(".config/llmc/config.toml");

        if local.try_exists().unwrap_or_default() {
            Some(local)
        } else if home_config.try_exists().unwrap_or_default() {
            Some(home_config)
        } else {
            None
        }
    };

    let Some(config_path) = config_path else {
        return Ok(LoadedConfig {
            file: ConfigFile::default(),
            base_dir: cwd.to_path_buf(),
        });
    };

    let contents = fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read config {}", config_path.display()))?;
    let file = toml::from_str(&contents)
        .with_context(|| format!("failed to parse config {}", config_path.display()))?;
    let base_dir = config_path.parent().unwrap_or(cwd).to_path_buf();

    Ok(LoadedConfig { file, base_dir })
}

fn command_output(command: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(command)
        .args(args)
        .output()
        .with_context(|| format!("failed to run {command}"))?;

    if !output.status.success() {
        bail!("{command} exited with {}", output.status);
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn resolve_path(path: &Path, cwd: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        let path = path.canonicalize()?;

        Ok(path)
    } else {
        let path = cwd.join(path).canonicalize()?;

        Ok(path)
    }
}

fn resolve_config_source(path: &Path, base_dir: &Path, home: &Path) -> Result<PathBuf> {
    let expanded = expand_config_home(path, home);
    if expanded.is_absolute() {
        expanded
            .canonicalize()
            .with_context(|| format!("failed to canonicalize path {}", expanded.display()))
    } else {
        {
            let path = base_dir.join(&expanded);
            path.canonicalize()
                .with_context(|| format!("failed to canonicalize path {}", expanded.display()))
        }
    }
}

fn expand_config_home(path: &Path, home: &Path) -> PathBuf {
    let path = path.to_string_lossy();
    if path == "~" {
        home.to_path_buf()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(path.as_ref())
    }
}
