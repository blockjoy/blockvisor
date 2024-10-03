const POLYGON_RPC_URL = "http://localhost:8545";

fn address(){
    run_sh(`heimdalld show-account | grep address | awk -F\" '{ print $4}'`).unwrap().to_string();
}

fn application_status() {
    let res = run_jrpc(#{
        host: `${global::POLYGON_RPC_URL}`,
        method: "eth_blockNumber",
        params: [],
        headers: [["Content-Type", "application/json"]],
      });
    if res.status_code != 200 {
       #{state: "delinquent", health: "healthy"}
    } else {
       #{state: "broadcasting", health: "healthy"}
    }
}

fn height(){
    parse_hex(run_jrpc(#{
        host: `${global::POLYGON_RPC_URL}`,
        method: "eth_blockNumber",
        params: [],
        headers: [["Content-Type", "application/json"]],
    }).expect(200).result)
}

fn block_age(){
    let res = run_jrpc(#{
        host: `${global::POLYGON_RPC_URL}`,
        method: "eth_getBlockByNumber",
        params: ["latest", false],
        headers: [["Content-Type", "application/json"]],
      }).expect(200);
    return 0;
}
