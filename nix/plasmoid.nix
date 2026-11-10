{
  runCommand,
}:

# Installs the Plasma 6 plasmoid to the standard location
# ($out/share/plasma/plasmoids/it.lapo.pitpat, §11).
runCommand "plasmoid-it.lapo.pitpat" { } ''
  mkdir -p $out/share/plasma/plasmoids
  cp -r ${../plasmoid/it.lapo.pitpat} $out/share/plasma/plasmoids/it.lapo.pitpat
''
