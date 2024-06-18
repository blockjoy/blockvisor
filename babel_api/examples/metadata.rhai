const METADATA = #{
    // A semver version of the babel program, indicating the minimum version of the babel
    // program that a babel script is compatible with.
    min_babel_version: "0.0.1",

    // Version of Linux kernel to use in VM.
    kernel: "5.10.174-build.1+fc.ufw",

    // A semver version of the blockchain node program.
    node_version: "1.2.3",

    // Name of the blockchain protocol.
    protocol: "example",

    // Type of the node (validator, beacon, etc).
    node_type: "node",

    // [optional] Some description of the node.
    description: "example blockchain node",

    // Blockchain resource requirements.
    requirements: #{
        // Virtual cores to share with VM
        vcpu_count: 1,

        // RAM allocated to VM in MB
        mem_size_mb: 2048,

        // Size of data drive for storing blockchain data (not to be confused with OS drive)
        disk_size_gb: 1,
    },

    // Supported blockchain networks.
    nets: #{
        // Key is the name of blockchain network
        test: #{
            // Url for given blockchain network.
            url: "https://test.example.blockchain/",

            // Blockchain network type.
            // Allowed values: "dev", "test", "main"
            net_type: "test",

            // [optional] Custom network metadata can also be added.
            seeds: "1500161dd491b67fb1ac81868952be49e2509c9f@52.78.36.216:26656,dd4a3f1750af5765266231b9d8ac764599921736@3.36.224.80:26656,8ea4f592ad6cc38d7532aff418d1fb97052463af@34.240.245.39:26656,e772e1fb8c3492a9570a377a5eafdb1dc53cd778@54.194.245.5:26656,6726b826df45ac8e9afb4bdb2469c7771bd797f1@52.209.21.164:26656",
        },
        main: #{
            url: "https://main.example.blockchain/",
            net_type: "main",
            seeds: "9df7ae4bf9b996c0e3436ed4cd3050dbc5742a28@43.200.206.40:26656,d9275750bc877b0276c374307f0fd7eae1d71e35@54.216.248.9:26656,1a3258eb2b69b235d4749cf9266a94567d6c0199@52.214.83.78:26656",
        },
    },

    // Configuration of Babel - agent running inside VM.
    babel_config: #{
        // Size of swap file created on the node, in MB.
        swap_size_mb: 512,

        // Location of swap file.
        swap_file_location: "/swapfile",

        // Set RAM disks inside VM
        ramdisks: [
            #{
                // Path to mount RAM disk to.
                ram_disk_mount_point: "/mnt/ramdisk",
                // RAM disk size, in MB.
                ram_disk_size_mb: 512,
            },
        ]

    },

    // Node firewall configuration.
    firewall: #{
        // Fallback action for inbound traffic used when packet doesn't match any rule.
        // Allowed values: "allow", "deny", "reject"
        default_in: "deny",

        // Fallback action for outbound traffic used when packet doesn't match any rule.
        // Allowed values: "allow", "deny", "reject"
        default_out: "allow",

        // Set of rules to be applied.
        rules: [
            #{
                // Unique rule name.
                name: "Allowed incoming tcp traffic on port",

                // Action applied on packet that match rule.
                // Allowed values: "allow", "deny", "reject"
                action: "allow",

                // Traffic direction for which rule applies.
                // Allowed values: "out", "in"
                direction: "in",

                // [optional] Protocol - ""both" by default.
                // Allowed values: "tcp", "udp", "both"
                protocol: "tcp",

                // Ip(s) compliant with CIDR notation.
                ips: "192.167.0.1/24",

                // List of ports. Empty means all.
                ports: [24567], // ufw allow in proto tcp port 24567
            },
            #{
                name: "Allowed incoming udp traffic on ip and port",
                action: "allow",
                direction: "in",
                protocol: "udp",
                ips: "192.168.0.1",
                ports: [24567], //ufw allow in proto udp from 192.168.0.1 port 24567
            },
        ],
    },
};
