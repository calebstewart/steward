# The documentation site at https://calebstew.art/steward.
#
# Built natively, not for Windows: a website is the one thing this flake
# makes that runs anywhere. `nix build .#docs` and the Pages workflow realize
# the same derivation, so what deploys cannot drift from what you preview.
{
  lib,
  stdenvNoCC,
  cacert,
  zola,
}:
stdenvNoCC.mkDerivation {
  pname = "steward-docs";
  version = "0.1.0";

  # Only docs/. The site does not depend on the crates, so a Rust change must
  # not rebuild it -- and `zola build` would otherwise see target/ and the rest
  # of the checkout in its source.
  src = lib.fileset.toSource {
    root = ../docs;
    fileset = lib.fileset.unions [
      ../docs/config.toml
      ../docs/content
      ../docs/static
      ../docs/templates
    ];
  };

  nativeBuildInputs = [ zola ];

  # Zola 0.23 builds an HTTP client up front for `load_data`, and panics if it
  # cannot load any CA certificates -- which a sandboxed build has none of. This
  # site fetches nothing; the certificates are only there so the client
  # constructs.
  SSL_CERT_FILE = "${cacert}/etc/ssl/certs/ca-bundle.crt";

  buildPhase = ''
    runHook preBuild
    zola build --output-dir ./public
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    cp -r ./public $out
    runHook postInstall
  '';

  meta = {
    description = "Documentation site for steward";
    homepage = "https://calebstew.art/steward";
    license = lib.licenses.mit;
    platforms = lib.platforms.all;
  };
}
