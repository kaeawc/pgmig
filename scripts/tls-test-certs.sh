#!/usr/bin/env bash
# Writes throwaway TLS material for the CI `tls` job into DIR:
#   ca.pem             CA that signs the server certificate
#   wrong.pem          an unrelated CA (verification against it must fail)
#   server.pem/.key    server certificate for DNS name `localhost` only (no IP address)
# Uses the openssl command-line tool; nothing here is linked into pgmig.
set -euo pipefail

dir=${1:?usage: tls-test-certs.sh DIR}
mkdir -p "$dir"

ca() { # file-stem common-name
  openssl req -new -newkey rsa:2048 -nodes -subj "/CN=$2" \
    -keyout "$dir/$1.key" -out "$dir/$1.csr" 2>/dev/null
  printf 'basicConstraints=critical,CA:TRUE\nkeyUsage=critical,keyCertSign,cRLSign\nsubjectKeyIdentifier=hash\n' \
    >"$dir/ca.ext"
  openssl x509 -req -in "$dir/$1.csr" -signkey "$dir/$1.key" -days 2 \
    -extfile "$dir/ca.ext" -out "$dir/$1.pem" 2>/dev/null
}

ca ca "pgmig test CA"
ca wrong "pgmig unrelated CA"

openssl req -new -newkey rsa:2048 -nodes -subj "/CN=localhost" \
  -keyout "$dir/server.key" -out "$dir/server.csr" 2>/dev/null
printf '%s\n' \
  'subjectAltName=DNS:localhost' \
  'basicConstraints=critical,CA:FALSE' \
  'keyUsage=critical,digitalSignature,keyEncipherment' \
  'extendedKeyUsage=serverAuth' \
  'authorityKeyIdentifier=keyid' >"$dir/server.ext"
openssl x509 -req -in "$dir/server.csr" -CA "$dir/ca.pem" -CAkey "$dir/ca.key" \
  -CAcreateserial -days 2 -extfile "$dir/server.ext" -out "$dir/server.pem" 2>/dev/null

openssl verify -CAfile "$dir/ca.pem" "$dir/server.pem"
