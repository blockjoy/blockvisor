# BlockVisor Host Setup Guide

## Prerequisites

 - `bvup` tool (see assets bellow)
 - `PROVISION_TOKEN` obtained from BlockJoy portal

### OS Requirements

BV doesn't require any particular linux distribution, but it is recommended to use:
`Ubuntu Server 18.04+`

#### Kernel Requirements

BV should work with any kernel version `>=4.14` and `<6.0.0`, but it is recommended to use at lease `5.10.0`.

### Install BV CLI Dependencies

Beside standard linux tool available in all distributions, BV requires following CLI tools to be available:

- `pigz`
- `tar`
- `fallocate`
- `debootstrap`
- `systemctl`
- `tmux`
- `ip`
- `mkfs.ext4`

For _Ubuntu_ based distributions:

```shell
apt update
apt install pigz debootstrap tmux util-linux e2fsprogs chrony
```

### Network Setup

Since BV uses Firecracker to run blockchain nodes, it requires bridge interface to be configured. 
If bridge interface name is different thant default `bvbr0`, then it must be explicitly passed to `bvup`
when provisioning(see `bvup --help` for more details).

See [firecracker docs](https://github.com/firecracker-microvm/firecracker/blob/main/docs/network-setup.md#advanced-setting-up-a-bridge-interface) for more details on network setup.

**Example:**

There are multiple ways to configure network depending on linux distribution and user preferences.

Below is example of `netplan` configuration:

```yaml
network:
  version: 2
  renderer: networkd
  ethernets:
    enp33s0:
      dhcp4: false
      dhcp6: false
  bridges:
    bvbr0:
      interfaces: [ enp33s0 ]
      addresses: [ 50.115.46.98/28 ]
      gateway4: 50.115.46.97
      nameservers:
        search: [ hosted.static.webnx.com ]
        addresses:
            - "1.1.1.1"
            - "8.8.8.8"
      parameters:
        stp: false
        forward-delay: 1
      dhcp4: no
      dhcp6: no
```
- Apply netplan configuration `netplan apply`
- Verify that the bridge `bvbr0` is the primary interface
- Reboot the server. Verify the network configuration persists through reboot and the server is reachable.

## Host Provisioning with `bvup`

Once everything described above is configured, run `bvup` to provision host and install BV: 

```sh
./bvup <PROVISION_TOKEN> [--region REGION]
```
where `<PROVISION_TOKEN>` is token obtained from BlockJoy portal.

See `bvup --help` for more details.

### Verify Installation 
After successfully running `bvup`, verify the config file is present within `/etc/blockvisor.json`
and run the following command to verify `BV` service status: 
```shell
systemctl status blockvisor.service
```

## [optional] Check auto-update

BV auto-update can be disabled (enabled by default)
by setting the following field in `/etc/blockvisor.json` config file:
```json
"update_check_interval_secs": null
```

Remember to restart BV service to apply new settings:
```shell
bv stop
bv start
```
