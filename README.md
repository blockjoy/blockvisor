# BlockVisor

The service that runs on the host systems and is responisble for provisioning and managing one or more blockchains on a single server.

## API proto files

API proto files are stored in [separate repository](https://github.com/blockjoy/api-proto).

Note that [git submodules](https://github.blog/2016-02-01-working-with-submodules/) are used to bring the protos to this project.

```
git submodule update --init --recursive
```

# High Level Overview

![](overview.jpg)

# Basic Scenarios
## Add Host - Host Provisioning

```mermaid
sequenceDiagram
    participant user as User
    participant frontend as Fronted
    participant backend as API
    participant host as Host
    participant bv as BlockvisorD
    
    user->>frontend: add new Host
    frontend->>backend: new Host
    backend-->>frontend: OTP
    frontend-->>user: provisioning script with OTP
    user->>host: run provisioning script with OTP
    host->>host: download images and start BlockvisorD with OTP
    bv->>backend: ProvisionHostRequest
    backend-->>bv: ProvisionHostResponse
```

## Add Node

### Overview

```mermaid
sequenceDiagram
    participant backend as API
    participant bv as BlockvisorD
    participant fc as Firecracker
    participant babel as Babel
    
    backend->>bv: NodeCreate
    bv-->>backend: InfoUpdate
    bv->>fc: create vm
    backend->>bv: NodeStart
    bv-->>backend: InfoUpdate
    bv->>fc: start vm with Babel inside
    babel->>fc: listen for messages on vsock
```

### More detailed view including key exchange and node initialization

```mermaid
sequenceDiagram
    participant frontend as Frontend
    participant backend as API
    participant bv as BV
    participant babel as Babel

    frontend ->> backend: Create Node
    backend ->> bv: Create Node
    bv ->> bv: Download os.img
    bv ->> bv: Create data.img

    frontend ->> backend: Start Node
    backend ->> bv: Start Node

    bv ->> babel: Start Node
    babel ->> babel: Load config
    babel ->> babel: Mount data.img
    babel ->> babel: Supervisor: wait for init

    bv ->> babel: Ping
    babel -->> bv: Pong
    bv ->> babel: Setup genesis block
    babel ->> babel: Init completed
    babel ->> babel: Start Supervisor
    babel ->> babel: Start blockchain

    bv ->> backend: Get keys
    backend -->> bv: Keys?
    alt Got keys
        bv ->> babel: Setup keys
    else No keys found
        bv ->> babel: Generate new keys
        babel -->> bv: Keys
        bv ->> backend: Save keys
    end

    opt Restart needed
        babel ->> babel: Supervisor: restart processes
    end

    frontend ->> backend: Get keys
    backend -->> frontend: Keys
```

## Execute Method on Blockchain

```mermaid
sequenceDiagram
    participant cli as BV CLI
    participant bv as BlockvisorD
    participant babel as Babel

    cli->>bv: Blockchain Method
    bv->>babel: send(method)
    babel->>babel: map method to Blockchain API as defined in config.toml
    babel-->>bv: response
    bv-->>cli: response
```
