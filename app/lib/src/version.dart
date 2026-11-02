/// Build identity, injected by the release pipeline:
///
///     flutter build linux --release --dart-define=PARLEY_VERSION=0.2.0
///
/// A source build without the define says `dev` — deliberately, so a
/// bug report that says "dev" is not mistaken for a released build.
const String parleyVersion = String.fromEnvironment(
  'PARLEY_VERSION',
  defaultValue: 'dev',
);

/// The commit this build was produced from, injected by the release
/// pipeline (`--dart-define=PARLEY_BUILD=...`). Empty for a source build.
///
/// A version number cannot see a release whose tag was moved and whose
/// artifacts were rebuilt; the commit can. The updater compares this with
/// the build recorded in the release's `RELEASE.json`.
const String parleyBuild = String.fromEnvironment(
  'PARLEY_BUILD',
  defaultValue: '',
);
