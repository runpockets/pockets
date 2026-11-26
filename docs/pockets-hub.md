# Pockets Hub

Pockets Hub is a lightweight GitHub index repository (`runpockets/pockets-hub`)
that lists curated, ready-to-pull Pocket applications shown on the **New pocket**
screen.

## Architecture: index repo + independent app repos

Each Pocket app stays in its **own** GitHub repository (for example,
`runpockets/coinkiwi`), with its own `pocket.yaml`, `Containerfile`, git
history, issues, and releases.

`runpockets/pockets-hub` only stores a single `catalog.json` file referencing
those repositories:

- **Live descriptions stay in sync:** When Pockets loads the Hub, it fetches
  each listed repository's root `pocket.yaml` (`raw.githubusercontent.com/<owner>/<repo>/HEAD/pocket.yaml`)
  and validates it with Pockets' canonical manifest parser. The card's title,
  version, and description come directly from `pocket.yaml` — the exact same
  metadata shown on the live Pocket card.
- **Offline & day-1 resilience:** Pockets caches the Hub catalog for 15 minutes
  and ships a bundled fallback (`crates/pockets-core/src/assets/hub-catalog.json`)
  containing CoinKiwi so new users still see featured Pockets offline.
- **No app release required to add Pockets:** Merging a PR into `catalog.json`
  makes the Pocket immediately discoverable by existing Pockets users.

## `catalog.json` format

Create `catalog.json` at the root of `runpockets/pockets-hub` on the `main`
branch:

```json
{
  "version": 1,
  "pockets": [
    {
      "repo": "runpockets/coinkiwi",
      "name": "CoinKiwi",
      "description": "Self-hosted crypto portfolio and price tracker.",
      "verified": true
    }
  ]
}
```

Field notes:
- `repo` *(required)*: Public GitHub repository slug (`owner/repo`).
- `name`, `description` *(optional fallback)*: Used only when the repo is
  unreachable or offline; overridden automatically by `metadata.name` and
  `metadata.description` from the repository's `pocket.yaml`.
- `verified` *(optional)*: Displays the `Verified` badge on the Hub card.

## Automated PR validation workflow for `runpockets/pockets-hub`

Add `.github/workflows/validate-catalog.yml` to `runpockets/pockets-hub` so
every PR automatically verifies that every listed repository exists, is public,
and contains both a valid `pocket.yaml` and a non-empty `Containerfile`:

```yaml
name: Validate Pockets Hub catalog

on:
  pull_request:
  push:
    branches: [main]

jobs:
  validate:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v4
        with:
          node-version: "22"
      - name: Validate every catalog entry
        run: |
          node -e '
          const fs = require("node:fs");
          const catalog = JSON.parse(fs.readFileSync("catalog.json", "utf8"));
          if (catalog.version !== 1 || !Array.isArray(catalog.pockets)) {
            throw new Error("catalog.json must have version: 1 and a pockets array");
          }
          (async () => {
            for (const entry of catalog.pockets) {
              if (!/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(entry.repo)) {
                throw new Error(`Invalid repo slug: ${entry.repo}`);
              }
              const base = `https://raw.githubusercontent.com/${entry.repo}/HEAD`;
              const [manifestRes, cfRes] = await Promise.all([
                fetch(`${base}/pocket.yaml`),
                fetch(`${base}/Containerfile`),
              ]);
              if (!manifestRes.ok) {
                throw new Error(`${entry.repo}: missing pocket.yaml (HTTP ${manifestRes.status})`);
              }
              if (!cfRes.ok || (await cfRes.text()).trim().length === 0) {
                throw new Error(`${entry.repo}: missing or empty Containerfile`);
              }
              const yaml = await manifestRes.text();
              if (!yaml.includes("apiVersion: pockets.dev/v1alpha1") || !yaml.includes("kind: Pocket")) {
                throw new Error(`${entry.repo}: pocket.yaml is missing apiVersion/kind`);
              }
              console.log(`OK: ${entry.repo}`);
            }
          })().catch((err) => { console.error(err); process.exit(1); });
          '
```

## Testing a custom catalog locally

Point the Pockets daemon at any HTTPS or loopback `catalog.json` using
`POCKETS_HUB_URL`:

```sh
POCKETS_HUB_URL=http://127.0.0.1:9000/catalog.json npm run dev:daemon
```
