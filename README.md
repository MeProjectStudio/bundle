# bundle

Declarative mod management system for Minecraft servers using OCI images

1. Define your plugin (or multiple plugins) as a **Bundle** using `Bundlefile` and build an OCI complaint image for your plugin
2. Push it to OCI registry (ghcr, docker, selfhosted)
3. Use `bundle server run` to pull an image and run your server with plugin versions from OCI registry


# Features
### Comes with CLI to build and push bundles
Create a `Bundlefile` file:

```
FROM scratch

ADD https://cdn.modrinth.com/data/1u6JkXh5/versions/JUWRHdru/worldedit-bukkit-7.4.1.jar plugins/worldedit.jar
```

Build it using `bundle build .` or add a tag to it `bundle build -t worldedit:latest .` or multiple `bundle build -t worldedit:latest -t worldedit:7.4.1 .`

See `bundle --help` for more

### **Multi-stage support**

Want to pull all the plugins and files you want in single image? `bundle` mirrors, docker multi-stage builds

```
FROM ghcr.io/someauthor/awesomeplugin:1.0.0 as awesomeplugin

COPY --from=awesomeplugin
```
