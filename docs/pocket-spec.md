# Pocket manifest: experimental M1 subset

`pocket.yaml` describes a single HTTP container. This is a deliberately narrow `pockets.dev/v1alpha1` development subset, not a stable public installation standard.

```yaml
apiVersion: pockets.dev/v1alpha1
kind: Pocket
metadata:
  id: hello-pocket
  name: Hello Pocket
  version: 0.1.0
  description: A small persistent web app you control.
runtime:
  image: localhost/pockets-hello:0.1.0
  architectures: [arm64, amd64]
web:
  port: 8080
  healthcheck: /health
storage:
  name: data
  path: /data
```

All fields above are required. Unknown fields are rejected. IDs and storage names are lowercase alphanumeric words separated by hyphens, at most 63 characters. Metadata strings must not be empty. Images must be nonempty, contain no whitespace, and not start with `-`. Architectures must be a nonempty, unique subset of `arm64` and `amd64`; execution must match the host architecture. Ports are integers from 1 through 65535.

The optional `previewApis` array declares up to 16 distinct public HTTPS
hostnames for Pockets preview. Each entry has an exact lowercase `host`, a
single-line `name` up to 80 characters, and a single-line `purpose` up to 240
characters. Declarations are portable. Pockets stores the user’s approvals
locally for the linked source. See [Preview API access](live-preview.md#preview-api-access).

Health checks are absolute HTTP paths, beginning with a single `/`, with no whitespace, fragments, or query strings. Storage paths are absolute container paths composed of ordinary alphanumeric, underscore, dot, or hyphen segments; `.` and `..` segments are prohibited. The bundled example remains fixed; locally imported source projects use this same manifest subset plus a reviewed, filtered build context. A manifest never supplies a host port, host mount, executable, arbitrary command, environment secret, or privilege flag.

The runtime allocates a loopback-only port and one engine-managed named volume. Stop preserves the volume. Managed project builds require explicit approval in Pockets; runtime launch pins the resulting immutable image ID instead of trusting the manifest's mutable image tag. The manifest never supplies a build command.

Schema: `packages/pocket-spec/pocket.schema.json`. Example: `examples/hello-pocket/pocket.yaml`. Both native validation and schema validation are required to evolve together.

General configuration, secrets, resource limits, publisher identity, digest policy, installed-app permissions, and multiple storage mounts are outside the manifest subset.
