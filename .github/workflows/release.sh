#!/bin/bash

bash -c "cargo install --path ."
export TAG=$(echo $2 | sed "s/refs\/tags\///")
export VERSION=$(echo $2 | sed "s/refs\/tags\/v//")
echo "Creating release for version $VERSION (tag $TAG)"
export RESPONSE=$(curl -L -X POST -H "Accept: application/vnd.github+json" -H "Authorization: Bearer $1" -H "X-GitHub-Api-Version: 2022-11-28" https://api.github.com/repos/netzbegruenung/all-llama-proxy/releases -d "{\"tag_name\":\"$TAG\",\"target_commitish\":\"main\",\"name\":\"$TAG\",\"body\":\"Version $VERSION\",\"draft\":false,\"prerelease\":false,\"generate_release_notes\":false}")
echo "$RESPONSE"
export RELEASE_ID=$(echo "$RESPONSE" | jq .id)
echo "Attaching file to release $RELEASE_ID"
echo "Build for x86_64:"
ls -lah ./target/release/
curl -L -X POST -H "Accept: application/vnd.github+json" -H "Authorization: Bearer $1" -H "Content-Type: application/octet-stream" --data-binary @"./target/release/all-llama-proxy" "https://uploads.github.com/repos/netzbegruenung/all-llama-proxy/releases/$RELEASE_ID/assets?name=all-lama-proxy"
echo "Finished: $?"
