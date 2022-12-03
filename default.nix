with import <nixpkgs> { };
let
  samply =
    rustPlatform.buildRustPackage
      rec {
        pname = "samply";
        version = "684d04c8f1463560a291fb6521070ce5c38c1138";

        src = ./.;
        nativeBuildInputs = [ pkg-config ];
        buildInputs = [ openssl ];

        cargoSha256 = "UYTlhV95GYFOFPZ15fsHlWBwp/guCe8yh7JiJB/AVwE=";
      };
in
mkShell
{
  name = "samply";
  buildInputs = [ samply firefox ];
}