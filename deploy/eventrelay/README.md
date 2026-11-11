# eventrelay on a k3s node

Store-and-forward relay. Producers POST events with a source token. The laptop
`nearpushd` pulls them with the consumer token and acks each batch. The relay runs in
the k3s namespace `example-relay` on a k3s node (x86_64). Every host name, namespace and
producer name in this directory is an example. Replace them with your own.

Placeholders used below:

- `<build-host>`: a machine that runs Docker (or OrbStack) and has this repository checked out.
- `<k8s-node>`: the k3s node that runs the relay. It is reached over SSH and has `sudo k3s`.
- `<relay-host>`: the node's MagicDNS name on your tailnet.
- `<tailnet>`: your tailnet's DNS suffix.

Files:

- `Dockerfile`: multi-stage build of the `eventrelay` binary on a distroless base, uid 65532.
- `Dockerfile.dockerignore`: keeps `target/`, `.agents/` and other local files out of the build context.
- `k8s/00-namespace.yaml` … `k8s/40-networkpolicy.yaml`: the manifests. The numeric prefixes
  make `kubectl apply -f k8s/` create the namespace first.

The manifests contain no token values. Create the two Secrets as described below.

## Build and import the image

Build on `<build-host>` and import into the node's k3s containerd. The image is tagged
`eventrelay:0.1.0`. k3s stores it as `docker.io/library/eventrelay:0.1.0`, which matches
the Deployment.

```sh
ssh <build-host> "cd ~/repo/nearpush && docker buildx build --platform linux/amd64 -f deploy/eventrelay/Dockerfile -t eventrelay:0.1.0 --load ."
ssh <build-host> "docker save eventrelay:0.1.0" | ssh <k8s-node> "sudo k3s ctr images import -"
ssh <k8s-node> "sudo k3s ctr images ls -q | grep eventrelay"
```

`imagePullPolicy` is `Never`, so the image must already be on the node before the Deployment starts.

The cargo step runs inside BuildKit, so an unchanged input set reuses a cached layer.

The `<build-host>` checkout must contain the committed tree. Copy it there first, for example
with `rsync -a --exclude .git --exclude .agents --exclude target ./ <build-host>:~/repo/nearpush/`.

## Create the namespace

Run this first. The Secrets in step (a) go into this namespace, and `kubectl create secret`
fails with "namespace not found" until it exists.

```sh
ssh <k8s-node> "sudo kubectl apply -f -" < deploy/eventrelay/k8s/00-namespace.yaml
```

## (a) Create the two Secrets

Values stay out of files and command lines where possible. The consumer token lives in the
laptop's login keychain. The source tokens live in one file on the laptop, mode 0600, because
the producers need them.

Do step (c) first. It creates the keychain item that holds the consumer token. Create the
namespace before this step (see "Create the namespace" above).

Create the consumer Secret from the keychain value. `security -w` writes the value to the
pipe, and `kubectl` reads it from stdin:

```sh
security find-generic-password -s nearpush-eventrelay-token -w \
  | ssh <k8s-node> "sudo kubectl -n example-relay create secret generic eventrelay-consumer-token --from-file=consumer-token=/dev/stdin"
```

Create the source-token Secret from a file. The file holds one `name:token` line per producer.
The name matches `[a-z0-9-]{1,32}`, and the token is at least 32 characters with no whitespace.
This example uses one producer named `producer-a`:

```sh
umask 077
mkdir -p ~/.config/nearpush-eventrelay
{ printf 'producer-a:'; openssl rand -hex 32; } > ~/.config/nearpush-eventrelay/source-tokens
chmod 600 ~/.config/nearpush-eventrelay/source-tokens

cat ~/.config/nearpush-eventrelay/source-tokens \
  | ssh <k8s-node> "sudo kubectl -n example-relay create secret generic eventrelay-source-tokens --from-file=source-tokens=/dev/stdin"
```

Keep `~/.config/nearpush-eventrelay/source-tokens`. Copy the `producer-a` token value into the
producer's own secret store. Do not delete the file before that copy is done.

To rotate a Secret, delete it on the node and create it again. Then restart the Deployment with
`sudo kubectl -n example-relay rollout restart deploy/eventrelay`.

Mode 0440 with group 65532 is the closest in-pod equivalent of 0600. kubelet writes Secret
files as root, so the file cannot be owned by uid 65532. With `fsGroup: 65532` the group is
65532, and only that group can read the file.

## Apply the workload manifests

Run this after both Secrets exist. The Deployment mounts both, and the pod stays in
`ContainerCreating` until they do. The namespace is already created, so skip
`00-namespace.yaml`. Apply the other files one at a time. Do not concatenate them with `cat`:
each file is a separate YAML document, and joining them without `---` separators produces an
invalid document.

```sh
for f in 10-pvc 20-service 30-deployment 40-networkpolicy; do ssh <k8s-node> "sudo kubectl apply -f -" < "deploy/eventrelay/k8s/$f.yaml"; done
ssh <k8s-node> "sudo kubectl -n example-relay rollout status deploy/eventrelay --timeout=120s"
```

## (b) Expose the consumer side on the node with tailscale serve

The Service is ClusterIP only. `tailscale serve` on the node forwards the tailnet HTTPS port
8443 to the Service's cluster address.

Read the ClusterIP:

```sh
ssh <k8s-node> "sudo kubectl -n example-relay get svc eventrelay -o jsonpath='{.spec.clusterIP}'; echo"
```

Check the existing serve config, then add the 8443 mapping:

```sh
ssh <k8s-node> "sudo tailscale serve status"
ssh <k8s-node> "sudo tailscale serve --bg --https=8443 http://<ClusterIP>:8080"
```

Port choice: 8443 is a non-default HTTPS port, so it does not share 443 with other serve
entries on the node. The laptop reaches the relay at `https://<relay-host>.<tailnet>.ts.net:8443`.
Replace `<tailnet>` with your tailnet's DNS suffix when you configure the laptop.

Producers that run inside the cluster use `http://eventrelay.example-relay.svc:8080`.
Producer POSTs that arrive through the tailnet path are refused with 403. The relay treats
any request with a `tailscale-*`, `x-forwarded-for`, `x-forwarded-host` or `forwarded`
header as proxied.

## (c) Set the nearpushd keychain item on the laptop

`nearpushd` reads the consumer token from the login keychain at startup:
`security find-generic-password -s nearpush-eventrelay-token -w`.

Create the item and leave the value off the command line, so `security` prompts for it:

```sh
security add-generic-password -s nearpush-eventrelay-token -a "$USER" -w
```

Generate the value first and paste it at the `password:` prompt. Copy it to the clipboard
without printing it, then clear the clipboard after the prompt:

```sh
openssl rand -hex 32 | pbcopy
security add-generic-password -s nearpush-eventrelay-token -a "$USER" -w
# paste with Cmd-V at the password: prompt, then:
pbcopy </dev/null
```

If the item already exists, add `-U` to update it. Update the node's Secret in the same way
(see the rotation note in step a), so both sides hold the same value.

Check that the item exists without printing it:

```sh
security find-generic-password -s nearpush-eventrelay-token >/dev/null && echo present
```

## (d) Tailnet ACL: limit port 8443 to the laptop

Only the laptop may reach the consumer port. In the tailnet policy file, tag the laptop
node and grant access to the relay node on port 8443 only. Tag names below are placeholders.
Replace them with the tags you use:

```json
"grants": [
  {
    "src": ["tag:nearpush-laptop"],
    "dst": ["tag:relay-node"],
    "ip": ["tcp:8443"]
  }
]
```

Check the rest of the policy as well. A broad rule such as `"src": ["*"], "dst": ["*"]`
grants every node access to port 8443, and the grant above does not remove that access.
Narrow or remove any such rule.

## Troubleshooting

- `/healthz` over the tailnet times out, while the in-cluster test from a producer pod
  works. The NetworkPolicy may drop traffic from the host. Read the Service's source address
  with `ip -4 addr show cni0` on the node, then add an `ipBlock` for that single /32 on port 8080
  to `k8s/40-networkpolicy.yaml`. Record the change here. Do not add a broader range.
- The pod is `CrashLoopBackOff` with a token file error. Check the Secret keys:
  `source-tokens` and `consumer-token`. Each token must be at least 32 characters.

## Rollback

```sh
# 1. Stop the tailnet path.
ssh <k8s-node> "sudo tailscale serve --https=8443 off"

# 2. Delete the workload and its network policy.
for f in 40-networkpolicy 30-deployment 20-service; do ssh <k8s-node> "sudo kubectl delete -f -" < "deploy/eventrelay/k8s/$f.yaml"; done

# 3. Delete the Secrets and the PVC. This deletes the stored events.
ssh <k8s-node> "sudo kubectl -n example-relay delete secret eventrelay-source-tokens eventrelay-consumer-token"
ssh <k8s-node> "sudo kubectl -n example-relay delete pvc eventrelay-data"

# 4. Optional: remove the namespace.
ssh <k8s-node> "sudo kubectl delete namespace example-relay"

# 5. On the laptop: remove the keychain item and the local source-token file.
security delete-generic-password -s nearpush-eventrelay-token
rm -f ~/.config/nearpush-eventrelay/source-tokens
```

Then remove the tailnet ACL grant from step (d).
