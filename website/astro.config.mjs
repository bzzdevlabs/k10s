import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

const repo = 'https://github.com/bzzdevlabs/k10s';

export default defineConfig({
	site: 'https://bzzdevlabs.github.io',
	base: '/k10s',
	trailingSlash: 'always',
	integrations: [
		starlight({
			title: 'k10s',
			description:
				'Run services on a fleet of machines for a fraction of what Kubernetes costs, by removing the layers instead of tuning them.',
			logo: { src: './src/assets/logo.svg', alt: 'k10s' },
			favicon: '/favicon.svg',
			social: [{ icon: 'github', label: 'GitHub', href: repo }],
			editLink: { baseUrl: `${repo}/edit/main/website/` },
			lastUpdated: true,
			disable404Route: true,
			customCss: ['@fontsource-variable/inter', '@fontsource-variable/jetbrains-mono', './src/styles/theme.css'],
			sidebar: [
				{
					label: 'Start here',
					items: [
						'start/introduction',
						'start/installation',
						'start/quick-start',
						'start/first-service',
						'start/cluster',
					],
				},
				{
					label: 'Concepts',
					items: [
						'concepts/architecture',
						'concepts/isolation',
						'concepts/instances',
						'concepts/cluster',
						'concepts/security',
					],
				},
				{
					label: 'The Ten language',
					items: [
						'ten/overview',
						'ten/types',
						'ten/syntax',
						'ten/builtins',
						'ten/prelude',
						'ten/compilation',
						'ten/examples',
					],
				},
				{
					label: 'Guides',
					items: [
						'guides/manifests',
						'guides/calls',
						'guides/storage',
						'guides/observability',
						'guides/production',
						'guides/bare-metal',
						'guides/editor',
					],
				},
				{
					label: 'Reference',
					items: ['reference/cli', 'reference/http-api', 'reference/host-api', 'reference/tir'],
				},
				{
					label: 'Project',
					items: ['project/benchmarks', 'project/releases', 'project/roadmap', 'project/contributing'],
				},
			],
		}),
	],
});
