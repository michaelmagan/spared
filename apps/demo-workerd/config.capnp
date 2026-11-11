using Workerd = import "/workerd/workerd.capnp";

const config :Workerd.Config = (
  services = [
    (name = "main", worker = .worker),
    (name = "disk", disk = (path = "../../bench/work/demo/workerd-data", writable = true)),
  ],
  sockets = [
    (name = "http", address = "127.0.0.1:9090", http = (), service = "main"),
  ],
);

const worker :Workerd.Worker = (
  modules = [
    (name = "worker.js", esModule = embed "worker.js"),
    (name = "page.html", text = embed "../demo-shared/page.html"),
  ],
  compatibilityDate = "2026-11-10",
  durableObjectNamespaces = [
    (className = "Items", uniqueKey = "demo-items", enableSql = true),
  ],
  durableObjectStorage = (localDisk = "disk"),
  bindings = [
    (name = "ITEMS", durableObjectNamespace = "Items"),
  ],
);
