/**
 * Which desktop OS the app runs on, put on <html data-platform> before the
 * first paint so globals.css can match that OS's font, cursor and selection
 * behaviour. Read from the user agent: WebKit on macOS says "Mac OS X",
 * WebView2 on Windows says "Windows".
 */
export type Platform = 'macos' | 'windows' | 'linux';

export const PLATFORM_BOOT_SCRIPT = `try{var u=navigator.userAgent;document.documentElement.dataset.platform=/Mac OS X|Macintosh/.test(u)?'macos':/Windows/.test(u)?'windows':'linux'}catch(e){}`;
