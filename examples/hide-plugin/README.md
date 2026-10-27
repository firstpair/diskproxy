# Example visibility plugin

A compiled reference policy that hides exact component names and filename
suffixes. It checks all path components, so a direct URL cannot access a child
of a hidden directory. Source bytes, timestamps, and volume identity use the
core's defaults.

The example is a Cargo workspace member and its tests run with the project.
It is **not selected by the stock application**. To register its `hide` name in
an experimental checkout, follow [Writing a plugin](../../docs/plugins.md) or
apply the included integration patch:

```sh
git apply --check examples/hide-plugin/register.patch
git apply examples/hide-plugin/register.patch
cargo test --workspace --locked
```

The patch adds the application dependency, plugin name, configuration validation,
construction branch, and lockfile entry. It was compiled and exercised against
synthetic sources, including direct HTTP requests for hidden descendants.
