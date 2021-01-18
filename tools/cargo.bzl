"""A module defining a rule for invoking cargo"""

load("@rules_rust//rust/private:rustc.bzl", "get_linker_and_args")
load("@rules_rust//rust/private:utils.bzl", "find_cc_toolchain")
load("@rules_rust//cargo:cargo_build_script.bzl", "get_cc_compile_env")

def _get_cc_files_and_env(ctx):
    toolchain_tools = []
    env = {}

    # Pull in env vars which may be required for the cc_toolchain to work (e.g. on OSX, the SDK version).
    # We hope that the linker env is sufficient for the whole cc_toolchain.
    cc_toolchain, feature_configuration = find_cc_toolchain(ctx)
    linker, _, linker_env = get_linker_and_args(ctx, cc_toolchain, feature_configuration, None)
    env.update(**linker_env)
    env.update({"RUSTC_LINKER": linker})

    # MSVC requires INCLUDE to be set
    cc_env = get_cc_compile_env(cc_toolchain, feature_configuration)
    include = cc_env.get("INCLUDE")
    if include:
        env["INCLUDE"] = include

    if not cc_toolchain:
        fail("A cc toolcahin is required")

    cc_executable = cc_toolchain.compiler_executable
    if cc_executable:
        env["CC"] = cc_executable
    ar_executable = cc_toolchain.ar_executable
    if ar_executable:
        # Because rust uses ar-specific flags, use /usr/bin/ar in this case.
        # TODO: Remove this workaround once ar_executable always points to an ar binary.
        if "libtool" in str(ar_executable):
            ar_executable = "/usr/bin/ar"
        env["AR"] = ar_executable

    return cc_toolchain.all_files, env

def _cargo_implementation(ctx):
    toolchain = ctx.toolchains["@rules_rust//rust:toolchain"]
    target_dir = ctx.actions.declare_directory("cargo/target")

    rustc_lib_dirs = depset([
        lib.dirname
        for lib in toolchain.rustc_lib.files.to_list()
    ]).to_list()

    cc_files, cc_env = _get_cc_files_and_env(ctx)

    rustflags = " ".join([
        "--verbose",
        "--codegen ar={}".format(cc_env["CC"]),
        "--codegen linker={}".format(cc_env["AR"]),
        "-L all={}".format(",".join([lib.path for lib in toolchain.rust_lib.files.to_list()])),
    ])

    env = {
        "CARGO": toolchain.cargo.path,
        "CARGO_TARGET_DIR": target_dir.path,
        "RUSTC": toolchain.rustc.path,
        "RUST_BACKTRACE": "full",
        "LD_LIBRARY_PATH": "{}:$LD_LIBRARY_PATH".format(":".join(rustc_lib_dirs)),
        "DYLD_LIBRARY_PATH": "{}:$DYLD_LIBRARY_PATH".format(":".join(rustc_lib_dirs)),
        "RUSTFLAGS": rustflags,
    }
    env.update(cc_env)

    tools = depset(
        [
            toolchain.cargo,
            toolchain.rustc,
        ],
        transitive = [
            cc_files,
            toolchain.rustc_lib.files,
            toolchain.rust_lib.files,
        ],
    )

    ctx.actions.run(
        executable = toolchain.cargo,
        outputs = [target_dir],
        inputs = ctx.files.data,
        env = env,
        arguments = ctx.attr.args,
        tools = tools,
    )

    return [DefaultInfo(
        files = depset([target_dir]),
    )]

cargo = rule(
    doc = "An internal wrapper rule for cargo commands",
    implementation = _cargo_implementation,
    attrs = {
        "args": attr.string_list(
            doc = "Arguments to pass to cargo",
            mandatory = True,
        ),
        "data": attr.label_list(
            doc = "Data to be made available for the cargo command",
            allow_files = True,
            mandatory = True,
        ),
        "_cc_toolchain": attr.label(
            default = Label("@bazel_tools//tools/cpp:current_cc_toolchain"),
        ),
    },
    fragments = ["cpp"],
    toolchains = [
        "@bazel_tools//tools/cpp:toolchain_type",
        "@rules_rust//rust:toolchain",
    ],
)
