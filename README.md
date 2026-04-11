# bundle
[![FOSSA Status](https://app.fossa.com/api/projects/git%2Bgithub.com%2FMeProjectStudio%2Fbundle.svg?type=shield)](https://app.fossa.com/projects/git%2Bgithub.com%2FMeProjectStudio%2Fbundle?ref=badge_shield)


Declarative mod management for Minecraft servers using OCI images.

Define your plugins in a `Bundlefile`, build them into standard OCI images, push to any
registry, and let `bundle` keep your server in sync.

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

## Config ownership with MANAGE

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


## License
[![FOSSA Status](https://app.fossa.com/api/projects/git%2Bgithub.com%2FMeProjectStudio%2Fbundle.svg?type=large)](https://app.fossa.com/projects/git%2Bgithub.com%2FMeProjectStudio%2Fbundle?ref=badge_large)