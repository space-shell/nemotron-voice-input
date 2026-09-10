{
  description = "nemotron-voice-input — streaming Nemotron voice input for Android (fork of notune/android_transcribe_app)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  inputs.flake-utils.url = "github:numtide/flake-utils";
  inputs.rust-overlay = {
    url = "github:oxalica/rust-overlay";
    inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          config = {
            allowUnfree = true;
            android_sdk.accept_license = true;
          };
          overlays = [ rust-overlay.overlays.default ];
        };
        # Nix-built Rust with the Android cross std baked in. A rustup-managed
        # toolchain does not execute under this system's loader arrangement.
        rust = pkgs.rust-bin.stable.latest.default.override {
          targets = [ "aarch64-linux-android" ];
        };
        android = pkgs.androidenv.composeAndroidPackages {
          platformVersions = [ "35" ];
          buildToolsVersions = [ "35.0.0" "34.0.0" ];
          # The app's Rust/NDK build expects NDK 28.0.13004108 (see README).
          includeNDK = true;
          ndkVersions = [ "28.0.13004108" ];
          includeEmulator = false;
          includeSystemImages = false;
        };
        jdk = pkgs.jdk17;
      in {
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            jdk
            android-tools   # adb, fastboot
            scrcpy
            android.androidsdk
            # Rust cross-compilation for aarch64-linux-android
            rust
            cargo-ndk
            # transcribe-cpp-sys builds its C++ core through CMake
            cmake
            ninja
          ];
          ANDROID_HOME = "${android.androidsdk}/libexec/android-sdk";
          ANDROID_SDK_ROOT = "${android.androidsdk}/libexec/android-sdk";
          JAVA_HOME = "${jdk}";
          GRADLE_OPTS = "-Dorg.gradle.daemon=false";
          shellHook = ''
            # Resolve the versioned NDK directory and export all the
            # variables the gradle cargoNdkBuild task / cargo-ndk know about.
            NDK_DIR="$(ls -d "$ANDROID_HOME"/ndk/* 2>/dev/null | head -1)"
            if [ -n "$NDK_DIR" ]; then
              export ANDROID_NDK_HOME="$NDK_DIR"
              export ANDROID_NDK_ROOT="$NDK_DIR"
              export ANDROID_NDK="$NDK_DIR"
            fi
          '';
        };
      });
}
