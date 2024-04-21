# mojang-api

Mojang API proxy with automatic caching.

## Sysctl

You need to change your `sysctl.conf` file to allow this:

```bash
net.ipv4.ip_forward=1
net.ipv6.conf.all.forwarding=1
net.ipv6.ip_nonlocal_bind = 1
```

## Add ipv6 range to interface

```bash
ip route add local <ip here>::/64 dev eth0
```
