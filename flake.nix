{
  description = "metric_space_hardware: a Veryl hardware project";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      rust-overlay,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
        inherit (pkgs) lib;

        # Newest nightly (as of the locked rust-overlay) that ships all of these components
        rust-nightly = pkgs.rust-bin.stable.latest.default.override {
            extensions = [
              "rust-src"
              "rust-analyzer"
            ];
          }
        ;

        project = (lib.importTOML ./Veryl.toml).project;
        top = "${project.name}_Top";

        # Gateware for a Lattice iCEstick (iCE40HX1K-TQ144, 12 MHz oscillator):
        # Blinky on the LEDs plus a UART echo (see src/top.veryl).
        # Each stage is its own derivation, so a stage is only rebuilt when its
        # own inputs (sources, flags, tool versions, previous stage) change.

        # 1. Veryl -> SystemVerilog
        blinky-sv =
          pkgs.runCommand "blinky-sv"
            {
              nativeBuildInputs = [ pkgs.veryl ];
              src = lib.fileset.toSource {
                root = ./.;
                fileset = lib.fileset.unions [
                  ./Veryl.toml
                  ./src
                ];
              };
            }
            ''
              cp -r $src/. . && chmod -R u+w .
              # The sandbox HOME is unwritable; give veryl somewhere to put caches.
              export HOME=$TMPDIR
              veryl build
              mkdir -p $out
              cp target/*.sv $out/
            '';

        # 2. Synthesis (Yosys)
        blinky-synth = pkgs.runCommand "blinky-synth" { nativeBuildInputs = [ pkgs.yosys ]; } ''
          mkdir -p $out
          yosys -q -l $out/yosys.log \
            -p "synth_ice40 -top ${top} -json $out/blinky.json" \
            ${blinky-sv}/*.sv
        '';

        # 3. Place and route (nextpnr-ice40)
        blinky-pnr = pkgs.runCommand "blinky-pnr" { nativeBuildInputs = [ pkgs.nextpnr ]; } ''
          mkdir -p $out
          nextpnr-ice40 --hx1k --package tq144 --freq 12 \
            --pcf ${./constraints/icestick.pcf} \
            --json ${blinky-synth}/blinky.json \
            --asc $out/blinky.asc -l $out/nextpnr.log
        '';

        # 4. Bitstream (icepack), with the earlier stages' logs linked alongside
        blinky-icestick =
          pkgs.runCommand "blinky-icestick-${project.version}" { nativeBuildInputs = [ pkgs.icestorm ]; }
            ''
              mkdir -p $out
              icepack ${blinky-pnr}/blinky.asc $out/blinky.bin
              ln -s ${blinky-synth}/yosys.log ${blinky-pnr}/nextpnr.log $out/
            '';

        # Flash the bitstream to the iCEstick's SPI flash; extra args go to iceprog.
        prog-icestick = pkgs.writeShellApplication {
          name = "prog-icestick";
          runtimeInputs = [ pkgs.icestorm ];
          text = ''iceprog "$@" ${blinky-icestick}/blinky.bin'';
        };
      in
      {
        packages = {
          inherit
            blinky-sv
            blinky-synth
            blinky-pnr
            blinky-icestick
            ;
          default = blinky-icestick;
        };

        apps.default = {
          type = "app";
          program = lib.getExe prog-icestick;
          meta.description = "Flash the bitstream to an iCEstick";
        };

        # mkShell (not mkShellNoCC) so Verilator and rustc have a C toolchain/linker.
        devShells.default = pkgs.mkShell {
          inputsFrom = [
            blinky-sv
            blinky-synth
            blinky-pnr
            blinky-icestick
          ];
          packages = [
            pkgs.verilator
            rust-nightly # rustc, cargo, clippy, rustfmt, rust-src, rust-analyzer
          ];
        };
      }
    );
}
