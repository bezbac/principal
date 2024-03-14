# Building the Dockerfile without having nix installed (inside docker)

1. Starting the Docker container

```
docker run -it --rm -v "$(pwd):/app" nixos/nix
```

2. Enable flakes

```
echo 'experimental-features = nix-command flakes' >> /etc/nix/nix.conf
```

3. Switch to app directory

```
cd /app
```

4. Build the binary

```
nix build .#bin
```

5. Copy the binary to the output folder

```
cp -r $(readlink -f result) output
```

6. Build the Docker image

```
nix build .#dockerImage
```

7. Copy the Docker image to the output folder

```
cp $(readlink -f result) output/docker-image.tar.gz
```

# Importing the nix-built image into docker

```
docker load -i output/docker-image.tar.gz
```
