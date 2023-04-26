# GlareDB

Repository for the core GlareDB database.

## Structure

- **crates**: All rust crates for the database. 
- **scripts**: Miscellaneous scripts for CI and whatnot.
- **testdata**: Data that can be used for testing the system, including TPC-H queries.

## Running

An in-memory version of GlareDB can be started with the following command:

``` shell
cargo run --bin glaredb -- -v server -l
```

To connect, use any Postgres compatible client. E.g. `psql`:

``` shell
psql "host=localhost dbname=glaredb port=6543"
```

When prompted for a password, any password will do[^1].

### Running Metastore Separately

By default, when the glaredb command is provided the `-l` flag, an in-process
Metastore will be spun up. Metastore is the service responsible for managing
catalogs for databases. But it may also be ran seperately (which is done in
production).

The following two commands may be used to spin up the server and metastore
components seperately:

``` shell
cargo run --bin glaredb -- -v metastore
```

and

``` shell
cargo run --bin glaredb -- -v server -l -m http://localhost:6545
```

## Service Overview

### GlareDB Server

Responsible for query planning and execution for one or more databases. Depends
on a running Metastore for fetching the database catalog.

Command:

``` shell
glaredb server -l
```

### Metastore

Stores and manages the catalog for one or more databases.

Command:

``` shell
glaredb metastore
```

### Pgsrv proxy

The postgres protocol proxy. This proxy authenticates user connections with the
Cloud service. Note that this has a hard dependency on a running Cloud service
that knows where database are deployed.

Command:

``` shell
glaredb proxy --api-addr https://qa.glaredb.com
```

## Code Overview

### Binaries

The `glaredb` binary lives in the `glaredb` crate. This one binary is able to
start the GlareDB server, Metastore, or Pgsrv proxy.

### Postgres wire protocol

We aim to be Postgres wire protocol compatible. The implementation of the
protocol, along with the proxying code, lives in the `pgsrv` crate.

### SQL Execution

The `sqlexec` crate provides most of the code related to SQL execution,
including session states and catalogs. The functionality exposed through
`sqlexec` maps closely to what's needed within `pgsrv`.

### Data sources

Data sources are external sources that GlareDB can hook into via `CREATE
EXTERNAL TABLE ...` calls. Each data source lives in its own `datasource_*`
crate.

## Logging and Tracing

We use the [tracing](https://docs.rs/tracing/latest/tracing/) library for our logging needs. When running locally,
these logs are output in a human-readable format. When running through Cloud,
logs are output in a JSON format which makes them searchable in GCP's logging
dashboard.

For ease of debugging, each external connection that comes in is given a unique
connection ID (UUID), and each Postgres message we encounter from that
connection creates a new span including that connection ID and the message that
we're processing. What that means is for most logs in the system, there will be
an accompanying connection ID for each log as seen below:

``` text
2023-01-06T19:43:13.840561Z DEBUG glaredb-thread-7 ThreadId(09) glaredb_connection{conn_id=2e881011-b649-4490-a5e2-1f086f7cee2a}:pg_protocol_message{name="query"}: sqlexec::planner: crates/sqlexec/src/planner.rs:27: planning sql statement statement=SELECT 1
2023-01-06T19:43:13.844780Z TRACE glaredb-thread-7 ThreadId(09) glaredb_connection{conn_id=2e881011-b649-4490-a5e2-1f086f7cee2a}:pg_protocol_message{name="query"}: pgsrv::codec::server: crates/pgsrv/src/codec/server.rs:48: sending message msg=RowDescription([FieldDescription { name: "Int64(1)", table_id: 0, col_id: 0, obj_id: 0, type_size: 0, type_mod: 0, format: 0 }])
```
## Dependency Graph
Generated by rust-analyzer viewCrateGraph
![image](https://user-images.githubusercontent.com/2547411/219805638-573caa4e-897e-433c-88db-15e3142fb047.png)

## Tagging Releases

Tags should should follow the same format as Cloud (e.g. `v0.0.1`).

To prepare for a release, the steps are as follows:

1. Update `Cargo.toml` with the new version and merge to main.
2. Pull latest main and create tag that matches the version specified in
   `Cargo.toml`.
3. Push tag, wait for image to build.
4. Update the Cloud terraform to point to the newly tagged image.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md)

[^1]: GlareDB currently allows any password. Access restriction is done within
    the `pgsrv` proxy.
