#!/usr/bin/env bash
echo "How many nodes would you like to run? (EX. 3)"

read -r numnodes

echo "Starting Cluster with $numnodes nodes..."

k3d cluster create dev --registry-create dev-registry --agents "$numnodes"

echo "Cluster running with $numnodes nodes"

{
  echo "Setting up Devspace Context"
  kubectl create ns devspace
  devspace use namespace devspace
  echo "Devspace Successfully Initialized"
} || {
  echo Please install Devspace: https://devspace.sh/docs/getting-started/installation
}

printf "\nCLUSTER INFO"
kubectl cluster-info

printf "\nCLUSTER NODES"
kubectl get nodes

printf "\nCLUSTER NAMESPACES"
kubectl get namespaces

# trap ctrl-c and call ctrl_c()
printf "\nCTRL+C TO EXIT"
trap ctrl_c INT

function ctrl_c() {
        printf "\nSHUTTING DOWN CLUSTER...\n"
        k3d cluster delete dev
        printf "\n\nCLUSTER DELETION COMPLETE"
        exit 0
}

while true; do
    sleep 1
done
