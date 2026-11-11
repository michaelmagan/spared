using Workerd = import "/workerd/workerd.capnp";

const config :Workerd.Config = (
  services = [
    (name = "main", worker = .worker),
    (name = "disk", disk = (path = "../../bench/work/workloads/workerd-data", writable = true)),
  ],
  sockets = [
    (name = "http", address = "127.0.0.1:9095", http = (), service = "main"),
  ],
);

const worker :Workerd.Worker = (
  modules = [
    (name = "worker.js", esModule = embed "worker.js"),
    (name = "marked", esModule = embed "../../bench/work/workerd/node_modules/marked/lib/marked.esm.js"),
  ],
  compatibilityDate = "2026-11-10",
  durableObjectNamespaces = [
    (className = "Db", uniqueKey = "workloads-db", enableSql = true),
  ],
  durableObjectStorage = (localDisk = "disk"),
  bindings = [
    (name = "DB", durableObjectNamespace = "Db"),
  ],
);
