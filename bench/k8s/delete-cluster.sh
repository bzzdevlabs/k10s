#!/usr/bin/env bash
source "$(dirname "$0")/lib.sh"
kind delete cluster --name "$CLUSTER" --kubeconfig "$KUBECONFIG" 2>&1 || true
rm -f "$KUBECONFIG"
