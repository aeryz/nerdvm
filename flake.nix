{
  description = "nerdvm dev shell";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { nixpkgs, rust-overlay, ... }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      };
      toolchain = pkgs.rust-bin.stable.latest.default.override {
        extensions = [ "rust-src" "rust-analyzer" ];
        targets = [ "x86_64-unknown-none" ];
      };
      rustPlatform = pkgs.makeRustPlatform {
        cargo = toolchain;
        rustc = toolchain;
      };
    in {
      packages.${system}.build-guest = rustPlatform.buildRustPackage {
        pname = "guest";
        version = "0.1.0";
        src = ./.;
        cargoLock.lockFile = ./Cargo.lock;
        buildPhase = ''
          cargo build --release --offline -p guest --target x86_64-unknown-none
        '';
        installPhase = ''
          mkdir -p $out
          cp target/x86_64-unknown-none/release/guest $out/guest
        '';
        doCheck = false;
      };

      devShells.${system}.default = pkgs.mkShell {
        packages = [ toolchain ];
      };
    };
}
