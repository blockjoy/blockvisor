import axios from 'axios';
import { ROUTES } from 'consts/routes';
import { writable } from 'svelte/store';

export const hosts = writable([]);
export const selectedHosts = writable([]);

export const fetchAllHosts = async () => {
  const all_hosts = [
    {
      title: 'All Hosts',
      href: ROUTES.HOSTS,
      children: [],
    },
  ];

  const res = await axios.get('/api/hosts/fetchHosts');

  const sorted = res.data.hosts.sort((a, b) => b.node_count - a.node_count);

  all_hosts[0].children = sorted.map((item) => {
    return {
      title: item.name,
      location: item.location,
      href: ROUTES.HOST_GROUP(item.id),
      id: item.id,
    };
  });

  hosts.set(all_hosts);
};

export const fetchHostById = async (id: string) => {
  const res = await axios.get('/api/hosts/fetchHostById', { params: { id } });

  selectedHosts.set(res.data.host);
};

export const getHostById = async (id: string) => {
  const res = await axios.get('/api/hosts/fetchHostById', { params: { id } });

  return res.data.host;
};
