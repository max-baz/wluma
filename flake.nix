{
  inputs = {
    naersk.url = "github:nix-community/naersk/master";
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils, naersk }:
    let systems = [ "x86_64-linux" "aarch64-linux" ];
    in flake-utils.lib.eachSystem systems (system:
      let
        pkgs = import nixpkgs { inherit system; };
        naersk-lib = pkgs.callPackage naersk { };
        libs = with pkgs; [
          udev
          v4l-utils
          vulkan-loader
          dbus
          pipewire
        ];
      in
      {
        defaultPackage = naersk-lib.buildPackage {
          version = "unstable-${builtins.substring 0 8 self.lastModifiedDate}-${self.shortRev or self.dirtyShortRev or "dirty"}";
          src = ./.;
          meta.mainProgram = "wluma";
          nativeBuildInputs = with pkgs; [
            makeWrapper
            pkg-config
            rustPlatform.bindgenHook
            marked-man
          ];
          buildInputs = libs;
        };
        devShell = with pkgs; mkShell {
          buildInputs = [ cargo rustc rustfmt pre-commit rustPackages.clippy pkg-config ] ++ libs;
          LD_LIBRARY_PATH = "${lib.makeLibraryPath [ wayland ]}";
          VK_LAYER_PATH = "${pkgs.vulkan-validation-layers}/share/vulkan/explicit_layer.d";
          VK_INSTANCE_LAYERS = "VK_LAYER_KHRONOS_validation";
          VK_LOADER_DEBUG = "error,warn";
          RUST_SRC_PATH = rustPlatform.rustLibSrc;
          LIBCLANG_PATH = "${llvmPackages.libclang.lib}/lib";
          BINDGEN_EXTRA_CLANG_ARGS = [
            ''-I"${llvmPackages.libclang.lib}/lib/clang/${llvmPackages.libclang.version}/include"''
          ] ++ (map (a: ''-I"${a}/include"'') [
            glibc.dev
            libv4l.dev
          ]);
        };
      }
    );
}
