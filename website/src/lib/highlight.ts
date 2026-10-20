import { createHighlighter, type DecorationItem } from 'shiki';
import { langs } from '../grammars.mjs';

const highlighter = createHighlighter({ themes: ['dark-plus'], langs: [...langs, 'json', 'bash'] });

export async function highlight(code: string, lang: string, decorations: DecorationItem[] = []) {
	return (await highlighter).codeToHtml(code, { lang, theme: 'dark-plus', decorations });
}
