// File and folder icons shipped by the VS Code extension.
import ten from '../../../editors/vscode/icons/ten.svg?raw';
import k10s from '../../../editors/vscode/icons/k10s.svg?raw';
import folderK10s from '../../../editors/vscode/icons/folder-k10s.svg?raw';
import folderK10sOpen from '../../../editors/vscode/icons/folder-k10s-open.svg?raw';
import folderTen from '../../../editors/vscode/icons/folder-ten.svg?raw';
import folderTenOpen from '../../../editors/vscode/icons/folder-ten-open.svg?raw';

export const fileIcons: Record<string, string> = { ten, k10s };
export const folderIcons: Record<string, [string, string]> = {
	k10s: [folderK10s, folderK10sOpen],
	ten: [folderTen, folderTenOpen],
};
