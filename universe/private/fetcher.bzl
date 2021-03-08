"""A module defining the cargo_universe rules and macros"""


_EXECUTE_FAIL_MESSAGE = """\
Failed to execute command with exit code ({}).
--------command:
{}
--------stdout:
{}
--------stderr:
{}
"""

def _get_bootstrapped_resolver_path(repository_ctx):
    configurations = ["debug", "release"]
    resolver_source_dir = repository_ctx.path(repository_ctx.attr._resolver_toml_file).dirname

    for cfg in configurations:
        bootstrapped_binary = repository_ctx.path(
            str(resolver_source_dir) + "/" +
            "target/{}/{}".format(cfg, _resolver_name(repository_ctx))
        )

        if bootstrapped_binary.exists:
            return bootstrapped_binary

    return None

_BUILD_CONTENT = """\
package(default_visibility = ["//visibility:public"])

exports_files(["{binary}"])

filegroup(
    name = "bin",
    srcs = [
        "{binary}",
    ],
)
"""

_WORKSPACE_CONTENT = """\
workspace(name = "{}")
"""

def _resolver_name(repository_ctx):
    extension = ".exe" if "windows" in repository_ctx.os.name else ""
    return "crate_universe_resolver{}".format(extension)

def _download_resolver(repository_ctx, url, sha256):
    resolver_path = repository_ctx.path(_resolver_name(repository_ctx))
    repository_ctx.download(
        url = resolver_url,
        sha256 = resolver_sha,
        output = resolver_path,
        executable = True,
    )

def _crate_universe_resolver_fetcher_impl(repository_ctx):

    repository_ctx.file(
        path = "BUILD.bazel",
        content = _BUILD_CONTENT.format(
            binary = _resolver_name(repository_ctx),
        ),
    )
    repository_ctx.file(
        path = "WORKSPACE.bazel",
        content = _WORKSPACE_CONTENT.format(
            repository_ctx.name,
        ),
    )

    # For debugging or working on changes to the resolver, you can set something like:
    #   export CRATE_UNIVERSE_RESOLVER_URL=file:///path/to/rules_rust/cargo/crate_universe_resolver/target/release/resolver
    resolver_url = repository_ctx.os.environ.get("CRATE_UNIVERSE_RESOLVER_URL", None)

    # Users can also compile the resolver using cargo and have Bazel use that binary
    bootstrapped_binary = _get_bootstrapped_resolver_path(repository_ctx)

    if resolver_url:
        if resolver_url.startswith("file://")
            sha256_command = ["sha256sum", resolver_url[7:]]
            sha256_result = repository_ctx.execute(sha256_command)
            if sha256_result.return_code != 0:
                fail(_EXECUTE_FAIL_MESSAGE.format(
                    sha256_result.return_code,
                    sha256_command,
                    sha256_result.stdout,
                    sha256_result.stderr,
                ))
            resolver_sha256 = sha256_result.stdout[:64]
        else:
            resulver_sha256 = None

        _download_resolver(repository_ctx, resolver_url, resolver_sha256)
    elif bootstrapped_binary:
        resolver_path = bootstrapped_binary
    elif repository_ctx.attr.target:
        resolver_path = None
    elif repository_ctx.attr.url:
        resolver_path = None
    else:
        fail("No method for fetching a `crate_universe_resolver` binary")

    # Sanity check, ensure the resolver exists
    if not repository_ctx.path(_resolver_name(repository_ctx)).exists:
        fail("The resolver was not created")

    return 

crate_universe_resolver_fetcher = repository_rule(
    doc = "A rule for providing a path to the `cargo_universe_resolver` binary",
    implementation = _crate_universe_resolver_fetcher_impl,
    attrs = {
        "url": attr.string(
            "A url to the resolver",
        ),
        "sha256": attr.string(
            "The sha256 value of the binary to download via `url`",
        ),
        "target": attr.label(
            "The label of a resolver binary",
        ),
        "_resolver_toml_file": attr.label(
            allow_single_file = True,
            default = Label("//cargo/crate_universe_resolver:Cargo.toml"),
        ),
    },
)
