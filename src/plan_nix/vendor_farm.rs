//! Vendored crate sources as derivations whose store paths follow from
//! `Cargo.lock` alone.
//!
//! Each crates.io package becomes a `builtin:fetchurl` fixed-output
//! derivation keyed on the checksum that `Cargo.lock` records, an unpack
//! derivation, and one entry in a symlink farm that cargo reads as a
//! directory source. Every `.drv` path is computed client-side, so a warm
//! build asks the store whether the farm is already realised instead of
//! running `cargo vendor`, and the store memoises the farm by content.

use super::aterm::{collect_drv_refs, compute_drv_store_path, serialize_derivation_aterm};
use super::daemon::NixDaemonConn;
use super::derivation::{downstream_placeholder, self_placeholder};
use crate::nix_encoding::{compress_hash, hex_lower, nix_base32_encode};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

const CRATES_IO_SOURCES: [&str; 2] = [
    "registry+https://github.com/rust-lang/crates.io-index",
    "sparse+https://index.crates.io/",
];

/// One crates.io package pinned by `Cargo.lock`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LockedCrate {
    pub(crate) name: String,
    pub(crate) version: String,
    /// SHA-256 of the `.crate` archive, in lower-case hex.
    pub(crate) checksum: String,
}

impl LockedCrate {
    /// The directory name inside the farm. Cargo reads a directory source
    /// by scanning manifests, so the name only has to be unique.
    fn dir_name(&self) -> String {
        format!("{}-{}", self.name, self.version)
    }
}

/// The crates.io packages of `lock_text`. `None` when a package comes from
/// anywhere else than crates.io or a local path, such as a git repository,
/// because only crates.io packages carry a checksum to fetch by.
pub(crate) fn crates_io_packages(lock_text: &str) -> Result<Option<Vec<LockedCrate>>> {
    let doc: toml::Value = toml::from_str(lock_text).context("Failed to parse Cargo.lock")?;
    let packages = doc
        .get("package")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    let mut crates = Vec::new();
    for package in packages {
        let Some(source) = package.get("source").and_then(|s| s.as_str()) else {
            continue;
        };
        let field = |key: &str| package.get(key).and_then(|v| v.as_str()).map(String::from);
        let (Some(name), Some(version), Some(checksum)) =
            (field("name"), field("version"), field("checksum"))
        else {
            return Ok(None);
        };
        if !CRATES_IO_SOURCES.contains(&source) {
            return Ok(None);
        }
        crates.push(LockedCrate {
            name,
            version,
            checksum,
        });
    }
    crates.sort_by_key(LockedCrate::dir_name);
    crates.dedup();
    Ok(Some(crates))
}

/// The output path of a flat, SHA-256 fixed-output derivation, as Nix's
/// `makeFixedOutputPath` computes it.
pub(crate) fn fixed_output_path(name: &str, sha256_hex: &str) -> String {
    let inner = Sha256::digest(format!("fixed:out:sha256:{sha256_hex}:").as_bytes());
    let fingerprint = format!("output:out:sha256:{}:/nix/store:{name}", hex_lower(&inner));
    let outer = Sha256::digest(fingerprint.as_bytes());
    format!(
        "/nix/store/{}-{name}",
        nix_base32_encode(&compress_hash(&outer, 20))
    )
}

/// Store paths of the programs the unpack and farm builders run.
pub(crate) struct BuildTools {
    pub(crate) bash_path: String,
    pub(crate) bash_store: String,
    pub(crate) coreutils_store: String,
    pub(crate) tar_store: String,
    pub(crate) gzip_store: String,
    pub(crate) system: String,
}

impl BuildTools {
    /// The tools on `PATH`, or `None` when one of them is missing or lives
    /// outside the store.
    pub(crate) fn from_path(system: &str) -> Option<Self> {
        let store_root = |name: &str| -> Option<String> {
            let bin = super::util::which_command(name).ok()?;
            let store = bin.parent()?.parent()?.to_string_lossy().to_string();
            store.starts_with("/nix/store/").then_some(store)
        };
        let (bash_path, bash_store) = super::util::which_bash().ok()?;
        Some(Self {
            bash_path,
            bash_store,
            coreutils_store: store_root("mkdir")?,
            tar_store: store_root("tar")?,
            gzip_store: store_root("gzip")?,
            system: system.to_string(),
        })
    }
}

/// A derivation whose `.drv` path is known before it is registered.
pub(crate) struct PlannedDrv {
    pub(crate) file_name: String,
    pub(crate) aterm: Vec<u8>,
    pub(crate) refs: Vec<String>,
    pub(crate) path: String,
}

impl PlannedDrv {
    pub(crate) fn new(name: &str, json: &serde_json::Value) -> Result<Self> {
        let file_name = format!("{name}.drv");
        let aterm = serialize_derivation_aterm(json)?;
        let refs = collect_drv_refs(json);
        let ref_strs: Vec<&str> = refs.iter().map(String::as_str).collect();
        let path = compute_drv_store_path(&file_name, &aterm, &ref_strs);
        Ok(Self {
            file_name,
            aterm,
            refs,
            path,
        })
    }
}

fn fetch_drv_json(krate: &LockedCrate) -> (String, serde_json::Value) {
    let name = format!("{}.crate", krate.dir_name());
    let out = fixed_output_path(&name, &krate.checksum);
    let url = format!(
        "https://static.crates.io/crates/{0}/{0}-{1}.crate",
        krate.name, krate.version
    );
    let json = serde_json::json!({
        "name": name,
        "system": "builtin",
        "builder": "builtin:fetchurl",
        "args": [],
        "outputs": {"out": {
            "hashAlgo": "sha256",
            "method": "flat",
            "hash": krate.checksum,
            "path": out,
        }},
        "inputDrvs": {},
        "inputSrcs": [],
        "env": {
            "allowSubstitutes": "",
            "builder": "builtin:fetchurl",
            "executable": "",
            "name": name,
            "out": out,
            "outputHash": krate.checksum,
            "outputHashAlgo": "sha256",
            "outputHashMode": "flat",
            "preferLocalBuild": "1",
            "system": "builtin",
            "unpack": "",
            "url": url,
            "urls": url,
        },
    });
    (out, json)
}

fn unpack_drv_json(
    krate: &LockedCrate,
    fetch_drv: &str,
    archive: &str,
    tools: &BuildTools,
) -> serde_json::Value {
    // An empty `files` map makes cargo skip the per-file verification that
    // `cargo vendor` output would allow, the same shape nixpkgs'
    // `importCargoLock` writes.
    let script = format!(
        "set -e\n\
         export PATH={gzip}/bin\n\
         {coreutils}/bin/mkdir -p \"$out\"\n\
         {tar}/bin/tar -xzf \"$archive\" --strip-components=1 --no-same-owner -C \"$out\"\n\
         printf '{{\"files\":{{}},\"package\":\"%s\"}}' \"$checksum\" > \"$out/.cargo-checksum.json\"\n",
        gzip = tools.gzip_store,
        coreutils = tools.coreutils_store,
        tar = tools.tar_store,
    );
    serde_json::json!({
        "name": krate.dir_name(),
        "system": tools.system,
        "builder": tools.bash_path,
        "args": ["-c", script],
        "outputs": {"out": {"hashAlgo": "sha256", "method": "nar"}},
        "inputDrvs": {fetch_drv: {"outputs": ["out"], "dynamicOutputs": {}}},
        "inputSrcs": [
            tools.bash_store,
            tools.coreutils_store,
            tools.gzip_store,
            tools.tar_store,
        ],
        "env": {
            "allowSubstitutes": "",
            "archive": archive,
            "checksum": krate.checksum,
            "out": self_placeholder("out"),
            "preferLocalBuild": "1",
        },
    })
}

fn farm_drv_json(entries: &[(String, String)], tools: &BuildTools) -> Result<serde_json::Value> {
    let mut input_drvs = serde_json::Map::new();
    let mut targets = Vec::with_capacity(entries.len());
    let mut names = Vec::with_capacity(entries.len());
    for (dir_name, unpack_drv) in entries {
        input_drvs.insert(
            unpack_drv.clone(),
            serde_json::json!({"outputs": ["out"], "dynamicOutputs": {}}),
        );
        targets.push(downstream_placeholder(unpack_drv, "out")?);
        names.push(dir_name.clone());
    }
    let script = format!(
        "set -e\n\
         {coreutils}/bin/mkdir -p \"$out\"\n\
         read -r -a __targets <<< \"$crateOuts\"\n\
         mapfile -t __names <<< \"$crateNames\"\n\
         i=0\n\
         while [ \"$i\" -lt \"${{#__targets[@]}}\" ]; do\n\
           {coreutils}/bin/ln -s \"${{__targets[$i]}}\" \"$out/${{__names[$i]}}\"\n\
           i=$((i+1))\n\
         done\n",
        coreutils = tools.coreutils_store,
    );
    Ok(serde_json::json!({
        "name": "vendor",
        "system": tools.system,
        "builder": tools.bash_path,
        "args": ["-c", script],
        "outputs": {"out": {"hashAlgo": "sha256", "method": "nar"}},
        "inputDrvs": input_drvs,
        "inputSrcs": [tools.bash_store, tools.coreutils_store],
        "env": {
            "allowSubstitutes": "",
            "crateNames": names.join("\n"),
            "crateOuts": targets.join(" "),
            "out": self_placeholder("out"),
            "preferLocalBuild": "1",
        },
    }))
}

/// Every derivation of a vendor farm, in an order where each one comes
/// after its inputs, and the farm's own `.drv` path.
pub(crate) struct VendorFarm {
    pub(crate) drvs: Vec<PlannedDrv>,
    pub(crate) farm_drv: String,
}

pub(crate) fn plan_vendor_farm(crates: &[LockedCrate], tools: &BuildTools) -> Result<VendorFarm> {
    let mut drvs = Vec::with_capacity(crates.len() * 2 + 1);
    let mut entries = Vec::with_capacity(crates.len());
    for krate in crates {
        let (archive, fetch_json) = fetch_drv_json(krate);
        let fetch = PlannedDrv::new(&format!("{}.crate", krate.dir_name()), &fetch_json)?;
        let unpack = PlannedDrv::new(
            &krate.dir_name(),
            &unpack_drv_json(krate, &fetch.path, &archive, tools),
        )?;
        entries.push((krate.dir_name(), unpack.path.clone()));
        drvs.push(fetch);
        drvs.push(unpack);
    }
    let farm = PlannedDrv::new("vendor", &farm_drv_json(&entries, tools)?)?;
    let farm_drv = farm.path.clone();
    drvs.push(farm);
    Ok(VendorFarm { drvs, farm_drv })
}

/// Register every derivation in `drvs` that the store does not hold yet,
/// in order. Returns how many were added.
pub(crate) fn register_planned(drvs: &[PlannedDrv]) -> Result<usize> {
    let mut conn = NixDaemonConn::connect().context("Connecting to the Nix daemon")?;
    let paths: Vec<&str> = drvs.iter().map(|d| d.path.as_str()).collect();
    let valid = conn.query_valid_paths(&paths)?;
    let mut added = 0;
    for drv in drvs.iter().filter(|d| !valid.contains(&d.path)) {
        let refs: Vec<&str> = drv.refs.iter().map(String::as_str).collect();
        let path = conn.add_text_to_store(&drv.file_name, &drv.aterm, &refs)?;
        anyhow::ensure!(
            path == drv.path,
            "Registered {} at {}, expected {}",
            drv.file_name,
            path,
            drv.path
        );
        added += 1;
    }
    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn itoa() -> LockedCrate {
        LockedCrate {
            name: "itoa".into(),
            version: "1.0.15".into(),
            checksum: "4a5f13b858c8d314ee3e8f639011f7ccefe71f97f96e50151fb991f267928e2c".into(),
        }
    }

    /// Paths from `nix-instantiate` of the equivalent `builtin:fetchurl`
    /// derivation expression.
    #[test]
    fn fetch_drv_matches_nix() {
        let (out, json) = fetch_drv_json(&itoa());
        assert_eq!(
            out,
            "/nix/store/l9kmabypaxs6qnnfhhgj85kyc6dhlmcw-itoa-1.0.15.crate"
        );
        let planned = PlannedDrv::new("itoa-1.0.15.crate", &json).unwrap();
        assert_eq!(
            planned.path,
            "/nix/store/5wpfn6wh33qbfqdka6aww5cyv446hg6r-itoa-1.0.15.crate.drv"
        );
    }

    #[test]
    fn lockfile_packages_from_crates_io() {
        let lock = r#"
version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = ["itoa"]

[[package]]
name = "itoa"
version = "1.0.15"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "4a5f13b858c8d314ee3e8f639011f7ccefe71f97f96e50151fb991f267928e2c"
"#;
        assert_eq!(crates_io_packages(lock).unwrap(), Some(vec![itoa()]));
    }

    #[test]
    fn lockfile_with_a_git_package_is_not_handled() {
        let lock = r#"
version = 4

[[package]]
name = "dep"
version = "0.1.0"
source = "git+https://example.com/dep.git?rev=abc#abc"
"#;
        assert_eq!(crates_io_packages(lock).unwrap(), None);
    }

    #[test]
    fn farm_path_follows_from_the_lockfile() {
        let tools = BuildTools {
            bash_path: "/nix/store/00000000000000000000000000000000-bash/bin/bash".into(),
            bash_store: "/nix/store/00000000000000000000000000000000-bash".into(),
            coreutils_store: "/nix/store/11111111111111111111111111111111-coreutils".into(),
            tar_store: "/nix/store/22222222222222222222222222222222-gnutar".into(),
            gzip_store: "/nix/store/33333333333333333333333333333333-gzip".into(),
            system: "x86_64-linux".into(),
        };
        let a = plan_vendor_farm(&[itoa()], &tools).unwrap();
        let b = plan_vendor_farm(&[itoa()], &tools).unwrap();
        assert_eq!(a.farm_drv, b.farm_drv);
        let mut bumped = itoa();
        bumped.version = "1.0.16".into();
        bumped.checksum = "0".repeat(64);
        let c = plan_vendor_farm(&[bumped], &tools).unwrap();
        assert_ne!(a.farm_drv, c.farm_drv);
    }
}
