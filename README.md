# bundle

Declarative mod management for Minecraft servers using OCI images.

Define your plugins in a `Bundlefile`, build them into standard OCI images, push to any
registry, and let `bundle` keep your server in sync.


## Hold on, what's this for again?

It's hard to manage your plugins and mods for a server when there is like 50 of them. We decided to borrow from docker-compose and build a tool that allows to distribute your plugins using OCI-compaint images.

So you can define what mods and servers to install using this in root of your server:
```toml
bundles = [
  "ghcr.io/realkarmakun/mcmetrics-exporter-velocity:0.5.0"
  "ghcr.io/luckperms/luckperms:^5",
  "example.com/someotherauthor/coolplugin:latest",
]
```

Then you run your server using `bundle server run` and it will pull mentioned versions and apply images to file system as author of a bundle intended.

This brings image's reproducability and enforces docker versioning flavor upon plugins. 

## Cool I guess, but how are they distributed?

When author builds their image they make `Bundlefile` which has syntax similar to `Dockerfile`. 

Then using `bundle build` and `bundle push` an OCI image is built and it can be pushed to Github Packages, self-hosted solution (e.g. Harbor), or even DockerHub!

So any author that wants to make their plugin accessible through `bundle` can easily do so. Storage is basically free.

So if you just a server admin, you don't need some central server or orchestrator. Just use same config.

## Woah, but my <some plugin> doesn't support bundle

That's not a problem! Many programs don't have official docker image. 

Feel free write some CI/CD scripts, to download and build your own `Bundlefile`. As nice bonus you can even include not only just a jar but multiple plugins. 

It's your bundle for your server after all. (You would need a private registry though. For security)

## What else does it do?

It also manages your server startup:

```toml
bundles = [
  "ghcr.io/realkarmakun/mcmetrics-exporter-velocity:0.5.0"
  "ghcr.io/luckperms/luckperms:^5",
  "example.com/someotherauthor/coolplugin:latest",
]

[server]
run = ["java", "-Xms128M", "-Xmx5G", "-jar", "velocity.jar"]

deny-override = ["bundle", "bundle.exe", "bundle.lock", "bundle.toml", "server.jar"]
```

So single `bundle server run` will:
1. Pull images mentioned in bundles. If image is already in cache it will not be pulled (this can be done with `bundle server pull`)
2. Apply images to file system, can be done with `bundle server apply`. Take notice of `server.deny-override`, it's a list of files that bundles can not override. 
3. Run command from `server.run` and run the server with updated plugins

---

## Installation

Download the latest binary for your platform from the
[releases page](https://github.com/MeProjectStudio/bundle/releases):

| Platform | Archive |
|----------|---------|
| Linux x86\_64 | `bundle-x86_64-unknown-linux-musl.tar.gz` |
| Linux aarch64 | `bundle-aarch64-unknown-linux-musl.tar.gz` |
| macOS x86\_64 | `bundle-x86_64-apple-darwin.tar.gz` |
| macOS arm64 | `bundle-aarch64-apple-darwin.tar.gz` |
| Windows x86\_64 | `bundle-x86_64-pc-windows-msvc.zip` |

Extract the `bundle` binary and put it somewhere on your `PATH`.

To update an existing installation:

```sh
bundle selfupdate
```

---

## Quickstart — publishing a bundle

A bundle is an OCI image containing the plugin jars and config files you want to ship.

**1. Scaffold a Bundlefile**

```sh
bundle init
```

**2. Edit the generated `Bundlefile`**

```dockerfile
FROM scratch

# Download a plugin jar directly from Modrinth (checksum verified on first build)
ADD https://cdn.modrinth.com/data/.../worldedit-7.4.1.jar  plugins/worldedit.jar

# Optionally bundle default config alongside it
ADD ./config/worldedit/  plugins/WorldEdit/
```

**3. Build and tag**

```sh
bundle build -t ghcr.io/you/worldedit:7.4.1 -t ghcr.io/you/worldedit:latest
```

**4. Log in and push**

```sh
bundle login ghcr.io
bundle push ghcr.io/you/worldedit:latest
```

---

## Quickstart — running a server with bundles

**1. Scaffold a `bundle.toml`** in your server directory

```sh
bundle server init
```

**2. Edit `bundle.toml`** to reference the images you want installed

```toml
bundles = [
  "ghcr.io/you/worldedit:latest",
  "ghcr.io/someauthor/essentials:^2",
]

[server]
run = ["java", "-Xmx4G", "-jar", "server.jar", "nogui"]
```

**3. Pull, apply, and start**

```sh
bundle server run
```

Or run each step individually:

```sh
bundle server pull   # resolve tags → digests, write bundle.lock
bundle server apply  # extract bundle layers onto the server directory
bundle server diff   # preview what apply would change without doing it
```

---

## Multi-stage bundles

Stages let you compose images and share files between them, mirroring Docker
multi-stage builds:

```dockerfile
FROM scratch AS plugins

ADD https://cdn.modrinth.com/data/.../sodium-0.6.jar  mods/sodium.jar
ADD https://cdn.modrinth.com/data/.../iris-1.8.jar    mods/iris.jar

FROM scratch AS config

COPY --from=plugins  mods/sodium.jar  mods/sodium.jar
ADD  ./config/sodium/                 config/sodium/
```

---

## Config ownership with MANAGE (EXPERIMENTAL)

`MANAGE` lets a bundle declare which keys it owns in a config file. On
`bundle server apply`, declared keys are taken from the bundle; every other
key keeps its on-disk value so user edits are never clobbered:

```dockerfile
MANAGE plugins/MyPlugin/config.yml: settings.enabled, settings.max-players
```

---

## All commands

```
bundle init                      Scaffold a Bundlefile
bundle build [PATH]              Build an OCI image from a Bundlefile
bundle push <IMAGE:TAG>          Push the last build to a registry
bundle login <REGISTRY>          Save registry credentials

bundle server init               Scaffold a bundle.toml
bundle server pull               Resolve tags and download layers
bundle server apply              Overlay bundle files onto the server directory
bundle server diff               Preview pending changes without applying
bundle server run                pull + apply + exec the server process

bundle version                   Print version, target, and git revision
bundle selfupdate                Update the bundle binary to the latest release
```
