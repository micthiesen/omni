#!/bin/sh
# Installs the Boris-side omni scripts kept in this directory into
# ~/compose/bin on Boris, backing up the copies they replace.
#
#   sh deploy/boris/install.sh
#
# deploy_omni.sh runs from omni-deploy.timer every minute; the next tick uses
# the new copy. Each file is syntax-checked on Boris before it replaces the
# installed one.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
stamp=$(date +%Y%m%d-%H%M%S)
for file in deploy_omni.sh omni_smoke.py; do
  scp -q "$here/$file" "boris:compose/bin/.$file.new"
  case "$file" in
    *.sh) ssh boris "bash -n compose/bin/.$file.new" ;;
    *.py) ssh boris "/usr/bin/python3 -m py_compile compose/bin/.$file.new && rm -rf compose/bin/__pycache__/.$file*" ;;
  esac
  ssh boris "mkdir -p compose/backups/omni-scripts && { [ ! -f compose/bin/$file ] || cp -p compose/bin/$file compose/backups/omni-scripts/$file.$stamp; } && chmod 755 compose/bin/.$file.new && mv compose/bin/.$file.new compose/bin/$file"
  echo "installed $file"
done
