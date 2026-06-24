use serde_json::Value;
use std::path::{Path, PathBuf};

#[test]
fn plugin_json_files_are_valid_and_reference_packaged_surfaces() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    let plugin = read_json(&root.join(".codex-plugin/plugin.json"));
    assert_eq!(plugin["name"], "attest");
    assert_eq!(plugin["mcpServers"], "./.mcp.json");
    assert_eq!(plugin["hooks"], "./hooks/hooks.json");
    assert_eq!(plugin["skills"], "./skills/");
    assert!(root.join(".mcp.json").is_file());
    assert!(root.join("hooks/hooks.json").is_file());
    assert!(root.join("skills/attest/SKILL.md").is_file());

    let mcp = read_json(&root.join(".mcp.json"));
    assert_eq!(mcp["attest"]["command"], "attest");
    assert_eq!(mcp["attest"]["args"][0], "mcp-server");

    let hooks = read_json(&root.join("hooks/hooks.json"));
    let command = hooks["hooks"]["Stop"][0]["hooks"][0]["command"]
        .as_str()
        .expect("hook command is a string");
    assert!(command.contains("${PLUGIN_ROOT}/scripts/attest-stop-hook"));
    assert!(root.join("scripts/attest-stop-hook").is_file());

    let marketplace = read_json(&root.join(".agents/plugins/marketplace.json"));
    assert_eq!(marketplace["plugins"][0]["name"], "attest");
    assert_eq!(marketplace["plugins"][0]["source"]["path"], "./");
}

fn read_json(path: &Path) -> Value {
    let bytes = std::fs::read(path).unwrap_or_else(|error| {
        panic!("failed to read {}: {error}", path.display());
    });
    serde_json::from_slice(&bytes).unwrap_or_else(|error| {
        panic!("failed to parse {}: {error}", path.display());
    })
}
