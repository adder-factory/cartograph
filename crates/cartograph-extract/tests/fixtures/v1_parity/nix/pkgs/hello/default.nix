{ stdenv, lib, fetchurl }:
stdenv.mkDerivation rec {
  pname = "hello";
  version = "2.12";
  src = fetchurl {
    url = "mirror://gnu/hello/hello-${version}.tar.gz";
    sha256 = lib.fakeSha256;
  };
  meta = with lib; {
    license = licenses.gpl3Plus;
    platforms = platforms.all;
  };
}
