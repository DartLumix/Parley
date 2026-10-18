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
