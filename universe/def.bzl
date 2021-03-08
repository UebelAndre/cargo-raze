"""A module defining the cargo_universe rules and macros"""

# Yay hand-crafted JSON serialisation...
_INPUT_CONTENT_JSON_TEMPLATE = """\
{{
  "repository_name": {name},
  "packages": [
      {packages}
  ],
  "cargo_toml_files": {{
      {cargo_toml_files}
  }},
  "overrides": {{
      {overrides}
  }},
  "repository_template": "{repository_template}",
  "target_triples": [
    {targets}
  ],
  "cargo": "{cargo}"
}}"""

_EXECUTE_FAIL_MESSAGE = """\
Failed to generate crate universe with exit code ({}).
--------stdout:
{}
--------stderr:
{}
"""

def _get_bootstrapped_resolver_path(repository_ctx):
    configurations = ["debug", "release"]
    extension = ".exe" if "windows" in repository_ctx.os.name else ""
    resolver_source_dir = repository_ctx.path(repository_ctx.attr._resolver_toml_file).dirname

    for cfg in configurations:
        bootstrapped_binary = repository_ctx.path(
            str(resolver_source_dir) + "/" +
            "target/{}/crate_universe_resolver{}".format(cfg, extension)
        )

        if bootstrapped_binary.exists:
            return bootstrapped_binary

    return None

def _get_cargo_files(repository_ctx):
    if repository_ctx.os.name == "linux":
        toolchain_repo = "@rust_linux_x86_64"
    elif repository_ctx.os.name == "mac os x":
        toolchain_repo = "@rust_darwin_x86_64"
    else:
        fail("Unsupported os: " + repr(repository_ctx.os.name))

    labels = struct(
        cargo = Label(toolchain_repo + "//:bin/cargo"),
        rustc = Label(toolchain_repo + "//:bin/rustc"),
    )

    files = struct(
        cargo = repository_ctx.path(labels.cargo),
        rustc = repository_ctx.path(labels.rustc),
    )

    if not files.cargo.exists:
        fail("Failed to get `cargo` binary from '{}'".format(labels.cargo))

    if not files.rustc.exists:
        fail("Failed to get `rustc` binary from '{}'".format(labels.rustc))

    return files

def _crate_universe_resolve_impl(repository_ctx):
    """Entry-point repository to manage rust dependencies.

    General flow is:
    - Serialize user-provided rule attributes into JSON
    - Call the Rust resolver script. It writes a `defs.bzl` file in this repo containing the
      transitive dependencies as repo rules in a `pinned_rust_install()` macro.
    - The user then calls defs.bzl%pinned_rust_install().

    Environment Variables:
        RULES_RUST_UPDATE_CRATE_UNIVERSE_LOCKFILE: Re-pin the lockfile if `true`.
        RULES_RUST_CRATE_UNIVERSE_RESOLVER_URL_OVERRIDE: Override URL to use to download resolver binary - for local paths use a file:// URL.
    """

    lockfile_path = None
    if repository_ctx.attr.lockfile:
        lockfile_path = repository_ctx.path(repository_ctx.attr.lockfile)

    cargo_files = _get_cargo_files(repository_ctx)

    # Yay hand-crafted JSON serialisation...
    input_content = _INPUT_CONTENT_JSON_TEMPLATE.format(
        name = "\"{}\"".format(repository_ctx.attr.name),
        packages = ",\n".join([artifact for artifact in repository_ctx.attr.packages]),
        cargo_toml_files = ",\n".join(['"{}": "{}"'.format(ct, repository_ctx.path(ct)) for ct in repository_ctx.attr.cargo_toml_files]),
        overrides = ",\n".join(["\"{}\": {}".format(oname, ovalue) for (oname, ovalue) in repository_ctx.attr.overrides.items()]),
        repository_template = repository_ctx.attr.repository_template,
        targets = ",\n".join(['"{}"'.format(target) for target in repository_ctx.attr.supported_targets]),
        cargo = cargo_files.cargo,
    )

    input_path = "_{name}.json".format(name = repository_ctx.attr.name)
    repository_ctx.file(input_path, content = input_content)

    # For debugging or working on changes to the resolver, you can set something like:
    #   export CRATE_UNIVERSE_RESOLVER_URL=file:///path/to/rules_rust/cargo/crate_universe_resolver/target/release/resolver
    resolver_url = repository_ctx.os.environ.get("CRATE_UNIVERSE_RESOLVER_URL", None)

    # Users can also compile the resolver using cargo and have Bazel use that binary
    bootstrapped_binary = _get_bootstrapped_resolver_path(repository_ctx)

    if resolver_url and resolver_url.startswith("file://"):
        sha256_result = repository_ctx.execute(["sha256sum", resolver_url[7:]])
        resolver_sha = sha256_result.stdout[:64]

        resolver_path = repository_ctx.path("resolver")
        repository_ctx.download(
            url = resolver_url,
            sha256 = resolver_sha,
            output = resolver_path,
            executable = True,
        )
    elif bootstrapped_binary:
        resolver_path = bootstrapped_binary
    else:
        resolver_path = repository_ctx.path(repository_ctx.attr.resolver)

    args = [
        resolver_path,
        "--input_path",
        input_path,
        "--output_path",
        repository_ctx.path("defs.bzl"),
        "--repo-name",
        repository_ctx.attr.name,
    ]
    if lockfile_path != None:
        args.append("--lockfile")
        str(args.append(lockfile_path))
    if repository_ctx.os.environ.get("RULES_RUST_UPDATE_CRATE_UNIVERSE_LOCKFILE", "false") == "true":
        args.append("--update-lockfile")

    result = repository_ctx.execute(
        args,
        environment = {
            "RUST_LOG": "info",
            "CARGO": str(cargo_files.cargo),
            "RUSTC": str(cargo_files.rustc),
        },
    )
    repository_ctx.delete(input_path)
    if result.return_code != 0:
        fail(_EXECUTE_FAIL_MESSAGE.format(
            result.return_code,
            result.stdout,
            result.stderr,
        )
    if result.stderr:
        print("Output from resolver: " + result.stderr)

    repository_ctx.file("BUILD.bazel")
    repository_ctx.file("WORKSPACE.bazel")

_crate_universe = repository_rule(
    doc = "A repository rule for generating Rust Bazel dependencies from a crate registry",
    implementation = _crate_universe_impl,
    attrs = {
        "manifest": attr.string(
            doc = "",
        ),
        "cargo_toml_files": attr.label_list(
            doc = "",
        ),
        "registry_template": attr.string(
            doc = "",
            default = "https://crates.io/api/v1/crates/{crate}/{version}/download",
        ),
        "supported_targets": attr.string_list(
            doc = "",
            allow_empty = False,
        ),
        "lockfile": attr.label(
            doc = "",
            allow_single_file = [".lock", ".json"],
            mandatory = False,
        ),
        "resolver": attr.label(
            doc = "",
            executable = True,
            cfg = "host",
            default = "@crate_universe_resolver//:bin",
        ),
        "_resolver_toml_file": attr.label(
            allow_single_file = True,
            default = Label("//cargo/crate_universe_resolver:Cargo.toml"),
        ),
    },
)

def _encode_manifests(name, manifests):
    return native.json.encode(struct(
        name = name,
        manifests = manifests,
    ))

def crate_universe(name, manifests = [], manifest = None, **kwargs):


    _crate_universe(
        name = name
        manifest = _encode_manifests
    )

def crate(**kwargs):
    return struct(**kwargs)

def manifest(package = None, crates = [])
    if package == None:
        package = native.package_name()

    return struct(
        package = package,
        crates = crates,
    )
