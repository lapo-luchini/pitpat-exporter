# SPDX-License-Identifier: ISC
{
  runCommand,
  jq,
  version,
}:

# Installs the Plasma 6 plasmoid to the standard location
# ($out/share/plasma/plasmoids/it.lapo.pitpat, §11). R3: the committed
# metadata.json carries the plain release version; the installed one
# shows the build's version (About page), rewritten with jq on JSON
# rather than sed on text.
runCommand "plasmoid-it.lapo.pitpat"
{
  inherit version;
  nativeBuildInputs = [ jq ];
}
  ''
  mkdir -p $out/share/plasma/plasmoids
  cp -r ${../plasmoid/it.lapo.pitpat} $out/share/plasma/plasmoids/it.lapo.pitpat
  chmod -R u+w $out/share/plasma/plasmoids/it.lapo.pitpat
  jq '.KPlugin.Version = "${version}"' \
    $out/share/plasma/plasmoids/it.lapo.pitpat/metadata.json \
    > $out/share/plasma/plasmoids/it.lapo.pitpat/metadata.json.new
  mv $out/share/plasma/plasmoids/it.lapo.pitpat/metadata.json.new \
    $out/share/plasma/plasmoids/it.lapo.pitpat/metadata.json
''
