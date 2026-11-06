//! Reads/writes OneDrive's sysadmin exclusion policy lists in HKLM (requires elevation to write).
//!
//! Each list is a registry key whose values are `name == data == pattern` (REG_SZ), which is how
//! Group Policy "show contents" list policies are stored.

use std::collections::BTreeSet;
use std::io;

use anyhow::{Context, Result};
use winreg::RegKey;
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ};

use crate::rules::{Blocks, Entry};

const BASE: &str = r"SOFTWARE\Policies\Microsoft\OneDrive";
const FILES: &str = "EnableODIgnoreListFromGPO";
const FOLDERS: &str = "EnableODIgnoreFolderListFromGPO";

fn read_list(name: &str) -> Result<BTreeSet<String>> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    match hklm.open_subkey_with_flags(format!(r"{BASE}\{name}"), KEY_READ) {
        Ok(key) => Ok(key.enum_values().flatten().map(|(n, _)| n.to_lowercase()).collect()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(BTreeSet::new()),
        Err(e) => Err(e).with_context(|| format!("reading {BASE}\\{name}")),
    }
}

fn write_list(name: &str, want: &BTreeSet<String>) -> Result<()> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let path = format!(r"{BASE}\{name}");
    if want.is_empty() {
        return match hklm.delete_subkey_all(&path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("clearing {path} (run elevated)"))
            }
            _ => Ok(()),
        };
    }
    let (key, _) = hklm
        .create_subkey(&path)
        .with_context(|| format!("opening {path} for write (run from an elevated shell)"))?;
    let have: BTreeSet<String> = key.enum_values().flatten().map(|(n, _)| n).collect();
    for stale in have.iter().filter(|n| !want.contains(&n.to_lowercase())) {
        key.delete_value(stale)?;
    }
    for pat in want {
        key.set_value(pat, pat)?;
    }
    Ok(())
}

pub fn read() -> Result<Blocks> {
    let mut blocks = Blocks::new();
    for (list, is_dir) in [(FILES, false), (FOLDERS, true)] {
        blocks.extend(read_list(list)?.into_iter().map(|name| Entry { name, is_dir }));
    }
    Ok(blocks)
}

pub fn write(b: &Blocks) -> Result<()> {
    for (list, is_dir) in [(FILES, false), (FOLDERS, true)] {
        write_list(list, &b.iter().filter(|e| e.is_dir == is_dir).map(|e| e.name.clone()).collect())?;
    }
    Ok(())
}
