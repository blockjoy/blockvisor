# BlockVisor

The service that runs on the host systems and is responsible for provisioning and managing one or more blockchains on a single server.

## How to release a new version
1. Make sure you have installed:
   - `git-conventional-commits`: `npm install --global git-conventional-commits`
   - `cargo-release`: `cargo install cargo-release`
2. Run `cargo release --execute $(git-conventional-commits version)` 
3. CI `publish` workflow will then build a bundle and create a new GH release

## Host Setup

See [BlockVisor Host Setup Guide](host_setup_guide.md) for more details.

Published version of above guide with `bvup` tool can be found [here](https://github.com/blockjoy/bv-host-setup/releases).

## Babel Plugins

BV is blockchain agnostic system that uses plugin system to add support for specific blockchains. 

See [Rhai Plugin Scripting Guide](babel_api/rhai_plugin_guide.md) for more details.

## API proto files

API proto files are stored in [separate repository](https://github.com/blockjoy/api-proto).

Note that [git submodules](https://github.blog/2016-02-01-working-with-submodules/) are used to bring the protos to this project.

```
git submodule update --init --recursive
```

## Log Levels Policy
- `error` - internal BV error (potential bug) or nonrecoverable error that requires manual actions;
error should rise alert
- `warn` - abnormal events that BV is capable to handle, e.g. networking issues, node recovery;
may be caused by external errors, but BV should recover when external system get to normal
- `info` - main actions with minimum context, e.g. node created;
avoid for frequently recurring actions like sending node status
- `debug` - Detailed actions flow with variables, include recurring actions like sending node status;
used during debugging issues, not printed by default
- `trace` - debug messages used on development phase by devs

## Important Paths
### Host
- `/opt/blockvisor/blacklist` bundle versions that failed to be installed
- `/opt/blockvisor/current` symlink to current `<version>`
- `/opt/blockvisor/<version>/` whole bundle
- `/etc/blockvisor.json` generated by `bvup <PROVISION_TOKEN>`, but can be later modified
- `/etc/systemd/system/tmux.service`
- `/etc/systemd/system/blockvisor.service`
- `/var/lib/blockvisor/nodes.json`
- `/var/lib/blockvisor/nodes/<uuid>.json` node specific metadata
- `/var/lib/blockvisor/nodes/<uuid>.rhai` node specific Babel plugin
- `/var/lib/blockvisor/nodes/<uuid>.data` Babel plugin specific data
- `/var/lib/blockvisor/firecracker/<uuid>/` node specific firecracker data (e.g. copy of images)
- `/var/lib/blockvisor/images/<protocol>/<node_type>/<node_version>/` downloaded images cache

### Node
- `/usr/bin/babel`
- `/usr/bin/babelsup`
- `/usr/bin/babel_job_runner`
- `/etc/babelsup.conf`
- `/etc/babel.conf`
- `/etc/systemd/system/babelsup.service`
- `/var/lib/babel/jobs/<job_name>.cfg`
- `/var/lib/babel/jobs/<job_name>.status`
- `/var/lib/babel/logs.socket`

### Bundle
- `bundle/installer`
- `bundle/babel/`
- `bundle/blockvisor/`
- `bundle/firecracker/`

## Testing

See [BV tests](bv/tests/README.md) for more.

# High Level Overview

![](overview.jpg)

## Node Internals

![](node_internals.jpg)

## Basic Scenarios
### Add Host - Host Provisioning

```mermaid
sequenceDiagram
    participant user as User
    participant frontend as Fronted
    participant backend as API
    participant host as Host
    participant storage as Storage
    
    user->>frontend: add new Host
    frontend->>backend: new Host
    backend-->>frontend: PROVISION_TOKEN
    frontend-->>user: bvup with PROVISION_TOKEN
    user->>host: run bvup with PROVISION_TOKEN
    host->>backend: provision host
    host->> storage: download bundle
    host->>host: run bundle installer
```

### Add Node

#### Overview

```mermaid
sequenceDiagram
    participant backend as API
    participant bv as BlockvisorD
    participant fc as Firecracker
    participant babelsup as BabelSup
    
    backend->>bv: NodeCreate
    bv-->>backend: InfoUpdate
    bv->>fc: create vm
    backend->>bv: NodeStart
    bv-->>backend: InfoUpdate
    bv->>fc: start vm with BabelSup inside
    babelsup->>fc: listen for messages on vsock
```

#### More detailed view including key exchange and node initialization

```mermaid
sequenceDiagram
    participant frontend as Frontend
    participant backend as API
    participant bv as BV
    participant babel as Babel

    frontend ->> backend: Create Node
    backend ->> bv: Create Node
    bv ->> bv: Download os.img, kernel, babel.rhai
    bv ->> bv: Create data.img

    backend ->> bv: Start Node

    bv ->> babel: Start Node
    babel ->> babel: Mount data.img

    bv ->> backend: Get keys
    backend -->> bv: Keys?
    alt Got keys
        bv ->> babel: Setup keys
    else No keys found
        bv ->> babel: Generate new keys
        babel -->> bv: Keys
        bv ->> backend: Save keys
    end

    bv ->> bv: call init on Babel plugin  
    bv ->> babel: run_*, start_job, ...
    Note right of bv: forward run_*, start_job and other calls<br> to bebel, so it can be run on the node
    babel -->> bv: 
    Note right of bv: result is sent back to BVand processed<br>  by Babel plugin 
    
    frontend ->> backend: Get keys
    backend -->> frontend: Keys
```

### Execute Method on Blockchain

```mermaid
sequenceDiagram
    participant cli as BV CLI
    participant bv as BlockvisorD
    participant babel as Babel

    cli->>bv: Blockchain Method
    bv ->> bv: call method on Babel plugin  
    bv ->> babel: run_*, start_job, ...
    Note right of bv: forward run_*, start_job and other calls<br> to bebel, so it can be run on the node
    babel -->> bv: 
    Note right of bv: result is sent back to BVand processed<br>  by Babel plugin 
    bv-->>cli: response
```

### Self update processes

![](host_self_update.jpg)

#### Check for update

```mermaid
sequenceDiagram
    participant repo as Cookbook+Storage
    participant bv as BlockvisorD
    participant installer as bundle/installer
    
    loop configurable check interval
    bv ->> repo: check for update
        alt
            repo -->> bv: no updates
        else
            repo -->> bv: update available
            bv ->> repo: download latest bundle
            bv ->> installer: launch installer
        end
    end
```

#### BlockvisorD update

```mermaid
sequenceDiagram
    participant api as API
    participant bv as BlockvisorD
    participant installer as bundle/installer
    participant sysd as SystemD
    
    alt running version is not blacklisted (this is not rollback)
        installer ->> installer: backup running version
    end
    alt BlockvisorD is running
        installer ->> bv: notify about started update
        activate bv
        bv ->> api: change status to UPDATING
        bv ->> bv: finish pending actions    
        deactivate bv
    end
    installer ->> installer: switch current version (change symlinks)  
    installer ->> sysd: restart BlockvisorD service     
    sysd ->> bv: restart BlockvisorD service
    installer ->> bv: finalize update and get health check status
    activate bv
    alt update succeed
        bv ->> bv: update Babel on running nodes
        bv ->> api: change status to IDLE
        bv -->> installer: ok
        installer ->> installer: cleanup old version
    else update failed
        bv -->> installer: err
        installer ->> installer: mark this version as blacklisted
        alt was it rollback
            installer ->> api: send rollback error (broken host) notification
        else
            installer ->> installer: launch installer from backup version
        end
    end
    deactivate bv
```

#### Babel and JobRunner install/update

```mermaid
sequenceDiagram
    participant bv as BlockvisorD
    participant fc as Firecracker
    participant babelsup as BabelSup
    participant babel as Babel
    
    bv ->> fc: start node
    activate babelsup
    babelsup ->> babel: start Babel if exists
    activate babel
    Note right of babelsup: otherwise wait for startNewBabel request
    bv ->> bv: connect BabelSup  
    activate bv
    bv ->> babelsup: check Babel binary
    alt Babel checksum doesn't match
        bv ->> babelsup: send new Babel binary
        babelsup ->> babelsup: replace Babel binary
        activate babel
        babelsup -->> babel: request graceful shutdowan
        babel ->> babel: finish pending actions
        babel ->> babel: graceful shutdown
        deactivate babel
        babelsup ->> babel: restart
        activate babel
    end

    bv ->> babel: check JobRunner binary
    alt JobRunner checksum doesn't match
        bv ->> babel: send new JobRunner binary
        babel ->> babel: replace JobRunner binary
        Note right of babelsup: JobRunner perform blockchain action/entrypoint<br> so it is not restarted automatiaclly
    end
    
    deactivate babel
    deactivate babelsup
    deactivate bv
```
