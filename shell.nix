{
  pkgs ? import <nixpkgs> { },
}:

pkgs.mkShell rec {
  buildInputs = with pkgs; [
    alsa-lib
    cargo
    libGL
    libxkbcommon
    wayland
  ];
  nativeBuildInputs = [ pkgs.pkg-config ];
  LD_LIBRARY_PATH = "${pkgs.lib.makeLibraryPath buildInputs}";
}
