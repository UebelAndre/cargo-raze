
/// Add binary dependencies as workspace members to the given workspace root Cargo.toml file
fn inject_binaries_into_workspace(
    &self,
    root_toml: &Path,
  ) -> Result<()> {
    // Read the current manifest
    let mut manifest = {
      let content = fs::read_to_string(root_toml)?;
      cargo_toml::Manifest::from_str(content.as_str())?
    };

    // Parse the current `workspace` section of the manifest if one exists
    let mut workspace = match manifest.workspace {
      Some(workspace) => workspace,
      None => cargo_toml::Workspace::default(),
    };

    // Add the binary dependencies as workspace members to the `workspace` metadata
    for dep in binary_deps.iter() {
      workspace.members.push(dep.to_string());
    }

    // Replace the workspace metadata with the modified metadata
    manifest.workspace = Some(workspace);

    // Write the metadata back to disk.
    // cargo_toml::Manifest cannot be serialized direcly.
    // see: https://gitlab.com/crates.rs/cargo_toml/-/issues/3
    let value = toml::Value::try_from(&manifest)?;
    std::fs::write(root_toml, toml::to_string(&value)?).with_context(|| {
      format!(
        "Failed to inject workspace metadata to {}",
        root_toml.display()
      )
    })
  }