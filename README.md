# mojang-api

Mojang API proxy with automatic caching.

## Sysctl

You need to change your `sysctl.conf` file to allow this:

```bash
cat <<EOF > /etc/sysctl.d/10-mojangapi.conf
net.ipv4.ip_forward=1
net.ipv6.conf.all.forwarding=1
net.ipv6.ip_nonlocal_bind = 1
EOF
```

## Add ipv6 range to interface

```bash
ip route add local <ip here>::/64 dev eth0
```

## Docker daemon config

```bash
cat <<EOF > /etc/docker/daemon.json
{
  "ipv6": true,
  "fixed-cidr-v6": "2001:db8:1::/64",
  "experimental": true,
  "ip6tables": true
}
EOF
```
