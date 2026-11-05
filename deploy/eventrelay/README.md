# eventrelay on p1

Store-and-forward relay. Producers POST events with a source token. The laptop
`nearpushd` pulls them with the consumer token and acks each batch. The relay runs in
the k3s namespace `nearpush-relay` on p1 (x86_64).

Files:

- `Dockerfile`: multi-stage build of the `eventrelay` binary on a distroless base, uid 65532.
- `Dockerfile.dockerignore`: keeps `target/`, `.agents/` and other local files out of the build context.
- `k8s/00-namespace.yaml` … `k8s/40-networkpolicy.yaml`: the manifests. The numeric prefixes
  make `kubectl apply -f k8s/` create the namespace first.

The manifests contain no token values. Create the two Secrets as described below.

## Build and import the image

Build on macmini and import into p1's k3s containerd. The image is tagged
`eventrelay:0.1.0`. k3s stores it as `docker.io/library/eventrelay:0.1.0`, which matches
the Deployment.

```sh
ssh macmini "cd ~/repo/nearpush && ~/.orbstack/bin/docker buildx build --platform linux/amd64 -f deploy/eventrelay/Dockerfile -t eventrelay:0.1.0 --load ."
ssh macmini "~/.orbstack/bin/docker save eventrelay:0.1.0" | ssh p1 "sudo k3s ctr images import -"
ssh p1 "sudo k3s ctr images ls -q | grep eventrelay"
```

`imagePullPolicy` is `Never`, so the image must already be on p1 before the Deployment starts.

The `macmini` checkout must contain the committed tree. Copy it there first, for example
with `rsync -a --exclude .git --exclude .agents --exclude target ./ macmini:~/repo/nearpush/`.

## Apply the manifests

Run this after the Secrets exist (step a). Otherwise the pod stays in `ContainerCreating`.

```sh
for f in deploy/eventrelay/k8s/*.yaml; do ssh p1 "sudo kubectl apply -f -" < "$f"; done
ssh p1 "sudo kubectl -n nearpush-relay rollout status deploy/eventrelay --timeout=120s"
```

The loop applies one file at a time in name order, so the namespace is created first. Do not
concatenate the files with `cat`: each file is a separate YAML document, and joining them without
`---` separators produces an invalid document.

## (a) Create the two Secrets

Values stay out of files and command lines where possible. The consumer token lives in the
laptop's login keychain. The source tokens live in one file on the laptop, mode 0600, because
the producers need them.

Do step (c) first. It creates the keychain item that holds the consumer token.

Create the consumer Secret from the keychain value. `security -w` writes the value to the
pipe, and `kubectl` reads it from stdin:

```sh
security find-generic-password -s nearpush-eventrelay-token -w \
  | ssh p1 "sudo kubectl -n nearpush-relay create secret generic eventrelay-consumer-token --from-file=consumer-token=/dev/stdin"
```

Create the source-token Secret from a file. The file holds one `name:token` line per producer.
The name matches `[a-z0-9-]{1,32}`, and the token is at least 32 characters with no whitespace:

```sh
umask 077
mkdir -p ~/.config/nearpush-eventrelay
{ printf 'openab-pty:'; openssl rand -hex 32; } > ~/.config/nearpush-eventrelay/source-tokens
chmod 600 ~/.config/nearpush-eventrelay/source-tokens

cat ~/.config/nearpush-eventrelay/source-tokens \
  | ssh p1 "sudo kubectl -n nearpush-relay create secret generic eventrelay-source-tokens --from-file=source-tokens=/dev/stdin"
```

Keep `~/.config/nearpush-eventrelay/source-tokens`. Copy the `openab-pty` token value into the
producer's own secret store. Do not delete the file before that copy is done.

To rotate a Secret, delete it on p1 and create it again. Then restart the Deployment with
`sudo kubectl -n nearpush-relay rollout restart deploy/eventrelay`.

Mode 0440 with group 65532 is the closest in-pod equivalent of 0600. kubelet writes Secret
files as root, so the file cannot be owned by uid 65532. With `fsGroup: 65532` the group is
65532, and only that group can read the file.

## (b) Expose the consumer side on p1 with tailscale serve

The Service is ClusterIP only. `tailscale serve` on p1 forwards the tailnet HTTPS port
8443 to the Service's cluster address.

Read the ClusterIP:

```sh
ssh p1 "sudo kubectl -n nearpush-relay get svc eventrelay -o jsonpath='{.spec.clusterIP}'; echo"
```

Check the existing serve config, then add the 8443 mapping:

```sh
ssh p1 "sudo tailscale serve status"
ssh p1 "sudo tailscale serve --bg --https=8443 http://<ClusterIP>:8080"
```

Port choice: 8443 is a non-default HTTPS port, so it does not share 443 with other serve
entries on p1. The laptop reaches the relay at `https://p1.<tailnet>.ts.net:8443`.
Replace `<tailnet>` with your tailnet's DNS suffix when you configure the laptop.

Producers that run inside the cluster use `http://eventrelay.nearpush-relay.svc:8080`.
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

If the item already exists, add `-U` to update it. Update the p1 Secret in the same way
(see the rotation note in step a), so both sides hold the same value.

Check that the item exists without printing it:

```sh
security find-generic-password -s nearpush-eventrelay-token >/dev/null && echo present
```

## (d) Tailnet ACL: limit port 8443 to the laptop

Only the laptop may reach the consumer port. In the tailnet policy file, tag the laptop
node and grant access to `p1` on port 8443 only. Tag names below are placeholders.
Replace them with the tags you use:

```json
"grants": [
  {
    "src": ["tag:nearpush-laptop"],
    "dst": ["tag:homelab-p1"],
    "ip": ["tcp:8443"]
  }
]
```

Check the rest of the policy as well. A broad rule such as `"src": ["*"], "dst": ["*"]`
grants every node access to port 8443, and the grant above does not remove that access.
Narrow or remove any such rule.

## Troubleshooting

- `/healthz` over the tailnet times out, while the in-cluster test from an `openab-pty` pod
  works. The NetworkPolicy may drop traffic from the host. Read the Service's source address
  with `ip -4 addr show cni0` on p1, then add an `ipBlock` for that single /32 on port 8080
  to `k8s/40-networkpolicy.yaml`. Record the change here. Do not add a broader range.
- The pod is `CrashLoopBackOff` with a token file error. Check the Secret keys:
  `source-tokens` and `consumer-token`. Each token must be at least 32 characters.

## Rollback

```sh
# 1. Stop the tailnet path.
ssh p1 "sudo tailscale serve --https=8443 off"

# 2. Delete the workload and its network policy.
for f in 40-networkpolicy 30-deployment 20-service; do ssh p1 "sudo kubectl delete -f -" < "deploy/eventrelay/k8s/$f.yaml"; done

# 3. Delete the Secrets and the PVC. This deletes the stored events.
ssh p1 "sudo kubectl -n nearpush-relay delete secret eventrelay-source-tokens eventrelay-consumer-token"
ssh p1 "sudo kubectl -n nearpush-relay delete pvc eventrelay-data"

# 4. Optional: remove the namespace.
ssh p1 "sudo kubectl delete namespace nearpush-relay"

# 5. On the laptop: remove the keychain item and the local source-token file.
security delete-generic-password -s nearpush-eventrelay-token
rm -f ~/.config/nearpush-eventrelay/source-tokens
```

Then remove the tailnet ACL grant from step (d).
