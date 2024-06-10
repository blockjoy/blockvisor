use crate::src::utils::execute_sql_insert;

pub async fn setup_rbac(db_url: &str) {
    let queries = vec![
        "
        insert into role_permissions (role, permission)
        values
        ('blockjoy-admin', 'auth-admin-list-permissions'),
        ('blockjoy-admin', 'blockchain-admin-add-node-type'),
        ('blockjoy-admin', 'blockchain-admin-add-version'),
        ('blockjoy-admin', 'blockchain-admin-get'),
        ('blockjoy-admin', 'blockchain-admin-list'),
        ('blockjoy-admin', 'blockchain-admin-view-private'),
        ('blockjoy-admin', 'host-admin-get'),
        ('blockjoy-admin', 'host-admin-list'),
        ('blockjoy-admin', 'host-admin-update'),
        ('blockjoy-admin', 'mqtt-admin-acl'),
        ('blockjoy-admin', 'node-admin-create'),
        ('blockjoy-admin', 'node-admin-delete'),
        ('blockjoy-admin', 'node-admin-get'),
        ('blockjoy-admin', 'node-admin-list'),
        ('blockjoy-admin', 'node-admin-report'),
        ('blockjoy-admin', 'node-admin-restart'),
        ('blockjoy-admin', 'node-admin-start'),
        ('blockjoy-admin', 'node-admin-stop'),
        ('blockjoy-admin', 'node-admin-update-config'),
        ('blockjoy-admin', 'node-admin-update-status'),
        ('blockjoy-admin', 'org-admin-get'),
        ('blockjoy-admin', 'org-admin-list'),
        ('blockjoy-admin', 'org-admin-update'),
        ('blockjoy-admin', 'user-admin-filter'),
        ('blockjoy-admin', 'user-admin-get'),
        ('blockjoy-admin', 'user-admin-update');
        ",
        "
        insert into role_permissions (role, permission)
        values
        ('api-key-user', 'user-create'),
        ('api-key-user', 'user-delete'),
        ('api-key-user', 'user-filter'),
        ('api-key-user', 'user-get'),
        ('api-key-user', 'user-update'),
        ('api-key-user', 'user-billing-delete'),
        ('api-key-user', 'user-billing-get'),
        ('api-key-user', 'user-billing-update');
        ",
        "
        insert into role_permissions (role, permission)
        values
        ('api-key-org', 'org-create'),
        ('api-key-org', 'org-get'),
        ('api-key-org', 'org-list'),
        ('api-key-org', 'org-update'),
        ('api-key-org', 'org-provision-get-token'),
        ('api-key-org', 'org-provision-reset-token');
        ",
        "
        insert into role_permissions (role, permission)
        values
        ('api-key-host', 'host-create'),
        ('api-key-host', 'host-delete'),
        ('api-key-host', 'host-get'),
        ('api-key-host', 'host-list'),
        ('api-key-host', 'host-regions'),
        ('api-key-host', 'host-restart'),
        ('api-key-host', 'host-start'),
        ('api-key-host', 'host-stop'),
        ('api-key-host', 'host-update'),
        ('api-key-host', 'host-billing-get'),
        ('api-key-host', 'host-provision-create'),
        ('api-key-host', 'host-provision-get');
        ",
        "
        insert into role_permissions (role, permission)
        values
        ('api-key-node', 'api-key-create'),
        ('api-key-node', 'api-key-delete'),
        ('api-key-node', 'api-key-list'),
        ('api-key-node', 'api-key-regenerate'),
        ('api-key-node', 'api-key-update'),
        ('api-key-node', 'blockchain-get'),
        ('api-key-node', 'blockchain-list'),
        ('api-key-node', 'command-ack'),
        ('api-key-node', 'command-create'),
        ('api-key-node', 'command-get'),
        ('api-key-node', 'command-pending'),
        ('api-key-node', 'command-update'),
        ('api-key-node', 'discovery-services'),
        ('api-key-node', 'key-file-create'),
        ('api-key-node', 'key-file-list'),
        ('api-key-node', 'metrics-host'),
        ('api-key-node', 'metrics-node'),
        ('api-key-node', 'node-create'),
        ('api-key-node', 'node-delete'),
        ('api-key-node', 'node-get'),
        ('api-key-node', 'node-list'),
        ('api-key-node', 'node-report'),
        ('api-key-node', 'node-restart'),
        ('api-key-node', 'node-start'),
        ('api-key-node', 'node-stop'),
        ('api-key-node', 'node-update-config');
        ",
        "
        insert into role_permissions (role, permission)
        values
        ('email-invitation', 'invitation-accept'),
        ('email-invitation', 'invitation-decline'),
        ('email-invitation', 'user-create');
        ",
        "
        insert into role_permissions (role, permission)
        values
        ('email-registration-confirmation', 'auth-confirm');
        ",
        "
        insert into role_permissions (role, permission)
        values
        ('email-reset-password', 'auth-update-password');
        ",
        "
        insert into role_permissions (role, permission)
        values
        ('grpc-login', 'api-key-create'),
        ('grpc-login', 'api-key-delete'),
        ('grpc-login', 'api-key-list'),
        ('grpc-login', 'api-key-regenerate'),
        ('grpc-login', 'api-key-update'),
        ('grpc-login', 'auth-list-permissions'),
        ('grpc-login', 'auth-refresh'),
        ('grpc-login', 'auth-update-ui-password'),
        ('grpc-login', 'babel-notify'),
        ('grpc-login', 'blockchain-get'),
        ('grpc-login', 'blockchain-list'),
        ('grpc-login', 'blockchain-view-public'),
        ('grpc-login', 'bundle-list-bundle-versions'),
        ('grpc-login', 'bundle-retrieve'),
        ('grpc-login', 'command-ack'),
        ('grpc-login', 'command-create'),
        ('grpc-login', 'command-get'),
        ('grpc-login', 'command-pending'),
        ('grpc-login', 'discovery-services'),
        ('grpc-login', 'invitation-accept'),
        ('grpc-login', 'invitation-decline'),
        ('grpc-login', 'invitation-list'),
        ('grpc-login', 'key-file-create'),
        ('grpc-login', 'key-file-list'),
        ('grpc-login', 'metrics-host'),
        ('grpc-login', 'metrics-node'),
        ('grpc-login', 'mqtt-acl'),
        ('grpc-login', 'node-report'),
        ('grpc-login', 'org-create'),
        ('grpc-login', 'org-get'),
        ('grpc-login', 'org-list'),
        ('grpc-login', 'org-provision-get-token'),
        ('grpc-login', 'org-provision-reset-token'),
        ('grpc-login', 'subscription-list'),
        ('grpc-login', 'user-create'),
        ('grpc-login', 'user-delete'),
        ('grpc-login', 'user-filter'),
        ('grpc-login', 'user-get'),
        ('grpc-login', 'user-update'),
        ('grpc-login', 'user-billing-delete'),
        ('grpc-login', 'user-billing-get'),
        ('grpc-login', 'user-billing-update');
        ",
        "
        insert into role_permissions (role, permission)
        values
        ('grpc-new-host', 'auth-refresh'),
        ('grpc-new-host', 'babel-notify'),
        ('grpc-new-host', 'blockchain-archive-get-download'),
        ('grpc-new-host', 'blockchain-archive-has-download'),
        ('grpc-new-host', 'blockchain-archive-get-upload'),
        ('grpc-new-host', 'blockchain-archive-put-download'),
        ('grpc-new-host', 'blockchain-get'),
        ('grpc-new-host', 'blockchain-get-image'),
        ('grpc-new-host', 'blockchain-get-plugin'),
        ('grpc-new-host', 'blockchain-get-requirements'),
        ('grpc-new-host', 'blockchain-list'),
        ('grpc-new-host', 'blockchain-list-image-versions'),
        ('grpc-new-host', 'blockchain-view-public'),
        ('grpc-new-host', 'bundle-list-bundle-versions'),
        ('grpc-new-host', 'bundle-retrieve'),
        ('grpc-new-host', 'command-ack'),
        ('grpc-new-host', 'command-create'),
        ('grpc-new-host', 'command-get'),
        ('grpc-new-host', 'command-pending'),
        ('grpc-new-host', 'command-update'),
        ('grpc-new-host', 'discovery-services'),
        ('grpc-new-host', 'host-get'),
        ('grpc-new-host', 'host-list'),
        ('grpc-new-host', 'host-update'),
        ('grpc-new-host', 'kernel-retrieve'),
        ('grpc-new-host', 'key-file-create'),
        ('grpc-new-host', 'key-file-list'),
        ('grpc-new-host', 'metrics-host'),
        ('grpc-new-host', 'metrics-node'),
        ('grpc-new-host', 'mqtt-acl'),
        ('grpc-new-host', 'node-create'),
        ('grpc-new-host', 'node-delete'),
        ('grpc-new-host', 'node-get'),
        ('grpc-new-host', 'node-list'),
        ('grpc-new-host', 'node-report'),
        ('grpc-new-host', 'node-restart'),
        ('grpc-new-host', 'node-start'),
        ('grpc-new-host', 'node-stop'),
        ('grpc-new-host', 'node-update-config'),
        ('grpc-new-host', 'node-update-status');
        ",
        "
        insert into role_permissions (role, permission)
        values
        ('org-owner', 'org-delete');
        ",
        "
        insert into role_permissions (role, permission)
        values
        ('org-admin', 'host-billing-get'),
        ('org-admin', 'invitation-create'),
        ('org-admin', 'invitation-revoke'),
        ('org-admin', 'org-remove-member'),
        ('org-admin', 'org-update'),
        ('org-admin', 'subscription-create'),
        ('org-admin', 'subscription-delete'),
        ('org-admin', 'subscription-update');
        ",
        "
        insert into role_permissions (role, permission)
        values
        ('org-member', 'host-create'),
        ('org-member', 'host-delete'),
        ('org-member', 'host-get'),
        ('org-member', 'host-list'),
        ('org-member', 'host-regions'),
        ('org-member', 'host-restart'),
        ('org-member', 'host-start'),
        ('org-member', 'host-stop'),
        ('org-member', 'host-provision-create'),
        ('org-member', 'host-provision-get'),
        ('org-member', 'node-create'),
        ('org-member', 'node-delete'),
        ('org-member', 'node-get'),
        ('org-member', 'node-list'),
        ('org-member', 'node-report'),
        ('org-member', 'node-restart'),
        ('org-member', 'node-start'),
        ('org-member', 'node-stop'),
        ('org-member', 'node-update-config'),
        ('org-member', 'org-create'),
        ('org-member', 'org-get'),
        ('org-member', 'org-list'),
        ('org-member', 'org-remove-self'),
        ('org-member', 'org-provision-get-token'),
        ('org-member', 'org-provision-reset-token'),
        ('org-member', 'subscription-get');
        ",
        "
        insert into role_permissions (role, permission)
        values
        ('org-personal', 'host-billing-get'),
        ('org-personal', 'host-create'),
        ('org-personal', 'host-delete'),
        ('org-personal', 'host-get'),
        ('org-personal', 'host-list'),
        ('org-personal', 'host-regions'),
        ('org-personal', 'host-restart'),
        ('org-personal', 'host-start'),
        ('org-personal', 'host-stop'),
        ('org-personal', 'host-provision-create'),
        ('org-personal', 'host-provision-get'),
        ('org-personal', 'node-delete'),
        ('org-personal', 'node-get'),
        ('org-personal', 'node-list'),
        ('org-personal', 'node-report'),
        ('org-personal', 'node-restart'),
        ('org-personal', 'node-start'),
        ('org-personal', 'node-stop'),
        ('org-personal', 'node-update-config'),
        ('org-personal', 'org-create'),
        ('org-personal', 'org-get'),
        ('org-personal', 'org-list'),
        ('org-personal', 'org-provision-get-token'),
        ('org-personal', 'org-provision-reset-token'),
        ('org-personal', 'org-update'),
        ('org-personal', 'subscription-create'),
        ('org-personal', 'subscription-delete'),
        ('org-personal', 'subscription-get'),
        ('org-personal', 'subscription-update');
        ",
        "
        insert into role_permissions (role, permission)
        values
        ('view-developer-preview', 'blockchain-view-development');
        ",
    ];

    for query in queries {
        execute_sql_insert(db_url, query);
    }
}
