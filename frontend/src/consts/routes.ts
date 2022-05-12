export const ROUTES = {
  AUTH_LOGIN: '/api/auth/login',
  AUTH_REGISTER: '/api/auth/register',
  AUTH_LOGOUT: '/api/auth/logout',
  LOGIN: '/',
  REGISTER: '/register',
  DASHBOARD: '/app/dashboard',
  PROFILE_SETTINGS: '/app/profile/settings',
  HOSTS: '/app/hosts',
  HOST_ADD: '/app/host/add',
  HOST_GROUP: (id: string) => `/app/hosts/group/${id}`,
  HOST_DETAILS: (id: string) => `/app/hosts/group/${id}`,
  NODES: '/app/nodes',
  NODE_ADD: '/app/node/add',
  NODE_DETAILS: (id: string) => `/app/node/${id}`,
  NODE_GROUP: (id: string) => `/app/nodes/group/${id}`,
  FORGOT_PASSWORD: '/forgot-password',
  VERIFY: '/verify',
  BROADCASTS: '/app/broadcasts',
  BROADCAST_CREATE: '/app/broadcasts/add',
  ADMIN_CONSOLE_DASHBOARD: '/app/admin-console',
  ADMIN_CONSOLE_USER_MANAGEMENT: '/app/admin-console/user-management',
  ADMIN_CONSOLE_USER_EDIT: (id: string) =>
    `/app/admin-console/user-management/edit/${id}`,
  ADMIN_CONSOLE_INVOICE: (userId: string, id: string) =>
    `/app/admin-console/user-management/edit/${userId}/invoice/${id}`,
};
