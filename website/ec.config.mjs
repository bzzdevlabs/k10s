import { defineEcConfig } from '@astrojs/starlight/expressive-code';
import { langs } from './src/grammars.mjs';

export default defineEcConfig({
	shiki: { langs },
	styleOverrides: {
		borderRadius: '0.6rem',
		codeFontFamily: 'var(--sl-font-mono)',
		frames: { shadowColor: 'transparent' },
	},
});
