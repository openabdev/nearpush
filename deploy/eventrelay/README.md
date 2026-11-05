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

## Apply the manifests

```sh
sudo kubectl apply -f deploy/eventrelay/k8s/
sudo kubectl -n nearpush-relay rollout status deploy/eventrelay --timeout=120s
```

Run the `apply` after the Secrets exist (step a). Otherwise the pod stays in
`ContainerCreating`.

## (a) Create the two Secrets from files

Both files are plain text and must be readable only by you. Use `umask 077` so the shell
creates them with mode 0600.

Source tokens file, one `name:token` line per producer. The name matches
`[a-z0-9-]{1,32}`. The token is at least 32 characters with no whitespace:

```
openab-pty:<64 hex characters>
```

Consumer token file, one line holding the token only (at least 32 characters, no whitespace):

```
<64 hex characters>
```

Generate and create the Secrets on p1 without printing the values:

```sh
umask 077
{ printf 'openab-pty:'; openssl rand -hex 32; } > source-tokens
openssl rand -hex 32 > consumer-token

sudo kubectl -n nearpush-relay create secret generic eventrelay-source-tokens --from-file=source-tokens=source-tokens
sudo kubectl -n nearpush-relay create secret generic eventrelay-consumer-token --from-file=consumer-token=consumer-token

rm -f source-tokens consumer-token
```

Create the namespace first if it does not exist yet (`sudo kubectl apply -f k8s/00-namespace.yaml`).
Keep the consumer token value. Step (c) needs the same value on the laptop. The source
tokens go to the producers, not the laptop.

Mode 0440 with group 65532 is the closest in-pod equivalent of 0600. kubelet writes Secret
files as root, so the file cannot be owned by uid 65532. With `fsGroup: 65532` the group is
65532, and only that group can read the file.

## (b) Expose the consumer side on p1 with tailscale serve

The Service is ClusterIP only. `tailscale serve` on p1 forwards the tailnet HTTPS port
8443 to the Service's cluster address.

Read the ClusterIP:

```sh
sudo kubectl -n nearpush-relay get svc eventrelay -o jsonpath='{.spec.clusterIP}'; echo
```

Check the existing serve config, then add the 8443 mapping:

```sh
sudo tailscale serve status
sudo tailscale serve --bg --https=8443 http://<ClusterIP>:8080
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

Paste the consumer token from step (a) at the `password:` prompt. If the item already exists,
add `-U` to update it.

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
sudo tailscale serve --https=8443 off

# 2. Delete the workload and its network policy.
sudo kubectl delete -f deploy/eventrelay/k8s/40-networkpolicy.yaml -f deploy/eventrelay/k8s/30-deployment.yaml -f deploy/eventrelay/k8s/20-service.yaml

# 3. Delete the Secrets and the PVC. This deletes the stored events.
sudo kubectl -n nearpush-relay delete secret eventrelay-source-tokens eventrelay-consumer-token
sudo kubectl -n nearpush-relay delete pvc eventrelay-data

# 4. Optional: remove the namespace.
sudo kubectl delete namespace nearpush-relay

# 5. On the laptop: remove the keychain item.
security delete-generic-password -s nearpush-eventrelay-token
```

Then remove the tailnet ACL grant from step (d).
