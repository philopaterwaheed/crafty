use clap::{Parser, Subcommand};
use regex::Regex;

use reqwest::blocking::get;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashSet,
    fs::{self, File},
    path::PathBuf,
    process::Command,
};

#[derive(Parser)]
#[command(name = "crafty")]
#[command(about = "Tool to manage ArchCraft packages from GitHub", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Install a package from ArchCraft GitHub
    Install { package: String },
    /// Upgrade a previously installed package
    Upgrade { package: Option<String> },
    /// Search for a package in the ArchCraft GitHub repository
    Search { keyword: String },
    /// Remove a package from the system
    Remove { package: String },
    /// List all packages available in the ArchCraft GitHub repository
    List,
}

#[derive(Serialize, Deserialize, Debug, Default)]
struct PackageDb {
    packages: HashSet<String>,
}

impl PackageDb {
    fn path() -> PathBuf {
        dirs::home_dir()
            .unwrap()
            .join(".config")
            .join(".crafty")
            .join("installed.json")
    }

    fn load() -> Self {
        let path = Self::path();
        if path.exists() {
            let data = fs::read_to_string(&path).unwrap_or_default();
            serde_json::from_str(&data).unwrap_or_default()
        } else {
            Self::default()
        }
    }

    fn save(&self) {
        let path = Self::path();
        let dir = path.parent().unwrap();
        fs::create_dir_all(dir).unwrap();
        let data = serde_json::to_string_pretty(self).unwrap();
        fs::write(path, data).unwrap();
    }

    fn add(&mut self, pkg: &str) {
        self.packages.insert(pkg.to_string());
        self.save();
    }

    fn remove(&mut self, pkg: &str) {
        self.packages.remove(pkg);
        self.save();
    }

    fn contains(&self, pkg: &str) -> bool {
        self.packages.contains(pkg)
    }
}

fn main() {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Install { package } => install_package(package),
        Commands::Upgrade { package } => upgrade_package(package.as_deref().unwrap_or("")),
        Commands::Search { keyword } => search_repo(keyword),
        Commands::Remove { package } => remove_package(package),
        Commands::List => list_packages(),
    }
}

fn install_package(pkg: &str) {
    // Construct the base URL for the raw GitHub repository
    let base_url = "https://github.com/archcraft-os/pkgs/raw/refs/heads/main/x86_64/";

    // Attempt to find the correct package file by listing available files
    let package_file = match find_package_file(pkg) {
        Some(file) => file,
        None => {
            eprintln!("Package '{}' not found in the repository.", pkg);
            return;
        }
    };

    let url = format!("{}{}", base_url, package_file);
    let zst_path = format!("/tmp/{}", package_file);
    let tar_path = zst_path.replace(".zst", "");

    println!("Downloading from {}", url);
    let response = reqwest::blocking::get(&url).expect("Download failed");
    let bytes = response.bytes().expect("Failed to read bytes");
    fs::write(&zst_path, &bytes).expect("Failed to write file");

    // Validate the downloaded file
    if !is_valid_zst(&zst_path) {
        eprintln!("Downloaded file is not a valid zstd archive.");
        return;
    }

    println!("Trying to install using pacman...");
    let status = Command::new("sudo")
        .arg("pacman")
        .arg("-U")
        .arg(&zst_path)
        .status()
        .expect("Failed to run pacman");

    if !status.success() {
        println!("Pacman failed to install the .zst file. Trying to decompress and retry...");

        let unzstd_status = Command::new("unzstd")
            .arg(&zst_path)
            .arg("-o")
            .arg(&tar_path)
            .status()
            .expect("Failed to decompress zst");

        if !unzstd_status.success() {
            eprintln!("Failed to decompress .zst file");
            return;
        }

        let retry_status = Command::new("sudo")
            .arg("pacman")
            .arg("-U")
            .arg(&tar_path)
            .status()
            .expect("Failed to install decompressed tar");

        if !retry_status.success() {
            eprintln!("Pacman failed to install decompressed package");
            return;
        }
    }

    println!("✅ Installed: {}", pkg);

    let re = Regex::new(r"^(?P<name>.+)-\d+(\.\d+)*-\d+-[^-]+\.pkg\.tar\.zst$").unwrap();
    let pkg_real_name = re
        .captures(&package_file)
        .and_then(|caps| caps.name("name").map(|m| m.as_str().to_string()))
        .unwrap_or_else(|| package_file.to_string());
    let mut db = PackageDb::load();
    db.add(&pkg_real_name);
}

fn upgrade_package(pkg: &str) {
    let db = PackageDb::load();
    if pkg.is_empty() {
        for installed_pkg in db.packages.iter() {
            println!("Upgrading {}", installed_pkg);
            install_package(installed_pkg);
        }
    } else if db.contains(pkg) {
        println!("Upgrading {}", pkg);
        install_package(pkg);
    } else {
        println!("Package '{}' is not installed via archcraft-tool.", pkg);
    }
}

fn search_repo(keyword: &str) {
    println!("Searching for '{}' in ArchCraft GitHub...", keyword);
    match find_packages_by_keyword(keyword) {
        Some(packages) if !packages.is_empty() => {
            println!("Found packages:");
            for pkg in packages {
                println!("- {}", pkg);
            }
        }
        _ => {
            println!("No packages found for '{}'", keyword);
        }
    }
}

fn remove_package(pkg: &str) {
    println!("Removing package {}", pkg);

    let status = Command::new("sudo")
        .arg("pacman")
        .arg("-Rns")
        .arg(pkg)
        .status()
        .expect("Failed to remove package");

    if status.success() {
        println!("✅ Removed: {}", pkg);
        let mut db = PackageDb::load();
        db.remove(pkg);
    } else {
        eprintln!("Failed to remove package");
    }
}

fn list_packages() {
    println!("Fetching package list from ArchCraft GitHub...");
    match get_all_packages() {
        Some(pkgs) if !pkgs.is_empty() => {
            println!("Available packages ({} total):", pkgs.len());
            for pkg in pkgs {
                println!("- {}", pkg);
            }
        }
        _ => {
            println!("Failed to fetch package list.");
        }
    }
}

// Helper function to validate if a file is a valid zstd archive
fn is_valid_zst(path: &str) -> bool {
    if let Ok(magic) = fs::read(path) {
        magic.starts_with(&[0x28, 0xb5, 0x2f, 0xfd])
    } else {
        false
    }
}

fn find_package_file(pkg: &str) -> Option<String> {
    let url = "https://github.com/archcraft-os/pkgs/tree/main/x86_64";
    let resp = get(url).ok()?.text().ok()?;

    // Extract the embedded JSON
    let start_marker = r#"<script type="application/json" data-target="react-app.embeddedData">"#;
    let end_marker = "</script>";

    let start = resp.find(start_marker)? + start_marker.len();
    let end = resp[start..].find(end_marker)? + start;

    let json_str = &resp[start..end];
    let json: Value = serde_json::from_str(json_str).ok()?;

    // Navigate to tree.items
    let items = json.pointer("/payload/tree/items")?.as_array()?;

    // Regex to match the specific package
    let re = Regex::new(&format!(
        r"^(?:archcraft-)?{}-[\d\.]+-\d+-(any|x86_64)\.pkg\.tar\.zst$",
        regex::escape(pkg)
    ))
    .ok()?;

    for item in items {
        if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
            if re.is_match(name) {
                return Some(name.to_string());
            }
        }
    }

    None
}

fn find_packages_by_keyword(keyword: &str) -> Option<Vec<String>> {
    let url = "https://github.com/archcraft-os/pkgs/tree/main/x86_64";
    let resp = get(url).ok()?.text().ok()?;

    // Extract the embedded JSON
    let start_marker = r#"<script type="application/json" data-target="react-app.embeddedData">"#;
    let end_marker = "</script>";

    let start = resp.find(start_marker)? + start_marker.len();
    let end = resp[start..].find(end_marker)? + start;

    let json_str = &resp[start..end];
    let json: Value = serde_json::from_str(json_str).ok()?;

    // Navigate to tree.items
    let items = json.pointer("/payload/tree/items")?.as_array()?;

    // Regex to match package files and extract package name
    let pkg_re = Regex::new(r"^(?P<pkg_name>.+)-[\d\.]+-\d+-(any|x86_64)\.pkg\.tar\.zst$").ok()?;

    let mut matching_packages = Vec::new();
    for item in items {
        if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
            if let Some(captures) = pkg_re.captures(name) {
                if let Some(pkg_name) = captures.name("pkg_name") {
                    // Search only in the package name part (without version and extension)
                    if pkg_name.as_str().to_lowercase().contains(&keyword.to_lowercase()) {
                        matching_packages.push(name.to_string());
                    }
                }
            }
        }
    }

    Some(matching_packages)
}

fn get_all_packages() -> Option<Vec<String>> {
    let url = "https://github.com/archcraft-os/pkgs/tree/main/x86_64";
    let resp = get(url).ok()?.text().ok()?;

    // Extract the embedded JSON
    let start_marker = r#"<script type="application/json" data-target="react-app.embeddedData">"#;
    let end_marker = "</script>";

    let start = resp.find(start_marker)? + start_marker.len();
    let end = resp[start..].find(end_marker)? + start;

    let json_str = &resp[start..end];
    let json: Value = serde_json::from_str(json_str).ok()?;

    // Navigate to tree.items
    let items = json.pointer("/payload/tree/items")?.as_array()?;

    // Regex to match package files
    let pkg_re = Regex::new(r"^(.+)-[\d\.]+-\d+-(any|x86_64)\.pkg\.tar\.zst$").ok()?;

    let packages: Vec<String> = items
        .iter()
        .filter_map(|item| {
            item.get("name")
                .and_then(|n| n.as_str())
                .filter(|name| pkg_re.is_match(name))
                .map(|s| s.to_string())
        })
        .collect();

    Some(packages)
}