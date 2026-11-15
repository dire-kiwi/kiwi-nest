# Deploying from CI

`https://kn.dire.nz` and `registry.dire.nz` are LAN-only, so only runners on the LAN can deploy to `dire`: the
kiwicd agents on VM 204 (`runs-on: [self-hosted, site-deploy]`, kiwicd's workflows) and the GitHub Actions runners
there (such as `trackmania-deploy`). GitHub-hosted runners can't reach it. Runner notes:
`~/personal/proxmox/vms/204-runners.md`.

## Deploy tokens

One token per workflow, as narrow as it can be. Admin only, so run these from the workstation:

```sh
# Production: may deploy, delete, inspect, suspend, resume and read the logs of `docs` alone
kiwi-nest --cluster dire token create deploy --name docs-deploy --pattern docs

# Previews: may manage `code-pr*`, and name `code` as --from without being able to change it
kiwi-nest --cluster dire token create deploy --name code-previews --pattern 'code-pr*' --from code
```

- `--pattern` and `--from` repeat; `*` matches any run of characters. A token for `code-pr*` can't touch `code`.
- Deploy tokens may push and pull images in the cluster's registry (any repository: images are shared), so
  `kiwi-nest push` works in CI with the same token.
- They can't create, load or delete shared volumes, mount one `rw`, or touch nodes, tokens or users.
- The token is printed once, on stdout (the message goes to stderr). Send it straight into the secret store rather
  than into the conversation or a file, for example
  `kiwi-nest --cluster dire token create deploy --name X --pattern X | gh secret set KIWI_NEST_TOKEN -R OWNER/REPO`
  (for kiwicd, its own secrets). If the user wants to do that step themselves, give them the command.
- `token list` shows them (not the secrets); `token revoke ID` revokes one.

## Installing the CLI in a workflow

The CLI must be at least as new as the server, so install the release of the server's version, checked against the
release's `SHA256SUMS`. This is `~/personal/docs-deploy/.github/workflows/deploy.yml`'s step:

```yaml
env:
  KIWI_NEST_SERVER: https://kn.dire.nz

steps:
  - name: Install the kiwi-nest CLI
    env:
      VERSION: ${{ vars.KIWI_NEST_VERSION }}     # optional: a newer release than the server's
    run: |
      version=${VERSION:-$(curl -fsS "$KIWI_NEST_SERVER/v1/version" | jq -r .version)}
      version=${version#v}
      asset=kiwi-nest-$version-x86_64-unknown-linux-musl
      releases=https://github.com/dire-kiwi/kiwi-nest/releases/download/v$version
      dir=$RUNNER_TEMP/kiwi-nest
      rm -rf "$dir" && mkdir -p "$dir/bin"
      curl -fsSL -o "$dir/SHA256SUMS" "$releases/SHA256SUMS" ||
        { echo "::error::no kiwi-nest release v$version on GitHub (dire-kiwi/kiwi-nest)"; exit 1; }
      curl -fsSL -o "$dir/$asset" "$releases/$asset"
      (cd "$dir" && grep -E " \*?$asset\$" SHA256SUMS | sha256sum --check --strict)
      install -m 0755 "$dir/$asset" "$dir/bin/kiwi-nest"
      echo "$dir/bin" >> "$GITHUB_PATH"
      "$dir/bin/kiwi-nest" version

  - name: Deploy
    env:
      KIWI_NEST_TOKEN: ${{ secrets.KIWI_NEST_TOKEN }}
      # Never the runner's own kiwi-nest configuration.
      KIWI_NEST_CONFIG_DIR: ${{ runner.temp }}/kiwi-nest/config
    run: kiwi-nest deploy docs ./bundle
```

The CLI reads `KIWI_NEST_SERVER` and `KIWI_NEST_TOKEN` (the same as `--server` and `--token`), so the token never
appears on a command line. Other things worth copying from docs-deploy:

- A `concurrency` group per app without `cancel-in-progress`, so runs take turns and a deploy isn't cut off part
  way.
- The release script builds the bundle in a temporary directory, writes settings into an `env_file` in it (refusing
  `$` and newlines in values), deploys, then polls the app's URL until it answers.
- `deploy` replaces the app in place. Workflows never `delete` before deploying: that removes the data.

## Previews: the GitHub Action

kiwi-nest's repository is a GitHub Action (`action.yml`, `scripts/action.sh`). On a pull request event it installs
the CLI of the server's version, then deploys `APP-prNUMBER` `--from APP` with `--domain`, `--idle-timeout` and
`--no-backups` (and `--access` if given), or deletes the preview (and its data) once the pull request is closed. It reads `.number` and
`.action` from the event, so it also works under kiwicd.

```yaml
on:
  pull_request:
    types: [opened, synchronize, reopened, closed]
concurrency: preview-${{ github.event.number }}
jobs:
  preview:
    runs-on: [self-hosted, site-deploy]       # on the LAN, so it reaches kn.dire.nz
    steps:
      - uses: actions/checkout@v4
      # build the bundle into ./bundle here, unless the repository is the bundle
      - uses: dire-kiwi/kiwi-nest@v0.2.1      # a release's tag
        with:
          server: https://kn.dire.nz
          token: ${{ secrets.KIWI_NEST_PREVIEW_TOKEN }}   # --pattern 'code-pr*' --from code
          app: code
          domain: "pr{number}.code.dire.nz"
          access: lan                         # only the LAN, whatever name a request uses
          bundle: ./bundle
```

Inputs: `server`, `token`, `app` (required); `domain` (needed to deploy; `{name}` is the preview's name, `{number}`
the pull request's); `bundle` (default `.`); `name` (default `APP-prNUMBER`); `idle-timeout` (default `10m`, empty
keeps the bundle's); `backups` (default `false`); `access` (`lan` or `public`, from 0.2.0; empty keeps the
bundle's); `ca-cert`; `version` (default the server's); `command` (`deploy`
or `delete`, default from the event); `repository`. Outputs: `name`, `url`.
Use v0.2.1 or later: v0.2.0's `action.yml` doesn't parse, so every run using it fails before it starts.

The preview's bundle is usually production's, built from the pull request's code. If it carries
`node_labels`, they must match production's node (a preview always goes to production's nest), and keep previews
behind `auth` or LAN-only (`access: lan`): they hold a copy of production's data.

## Pushing images from CI

```sh
docker build --platform linux/amd64 -t app-deps:ci deps/
kiwi-nest push app-deps:ci app-deps:$(sha256sum deps/Dockerfile | cut -c1-12)   # prints registry.dire.nz/app-deps:…
```

`kiwi-nest push` logs Docker in to `registry.dire.nz` with the CLI's token. Tag dependency images by a hash of what
goes into them, so an unchanged image isn't pushed again, and deploy within 24 hours of a push (garbage
collection keeps unused tags for that long).
