export function parseBrowserArguments(args: string[]): { standalone: boolean; argv: string[] } {
  const own = args.filter((arg) => !arg.startsWith("--ozone-platform=") && !arg.startsWith("--screen-info="));
  const standalone = own.includes("--standalone");
  if (!standalone) {
    if (own.some((arg) => arg !== "--daemon")) throw new Error("use --standalone to launch a foreground browser session");
    return { standalone: false, argv: [] };
  }
  if (own.includes("--daemon")) throw new Error("--standalone and --daemon cannot be combined");
  const argv = own.filter((arg) => arg !== "--standalone");
  for (const arg of argv) {
    if (arg.startsWith("-") && arg !== "--allow-clipboard-read" && arg !== "--transparent") {
      throw new Error(`unsupported foreground browser option: ${arg}`);
    }
  }
  if (argv.filter((arg) => !arg.startsWith("-")).length > 1) throw new Error("the foreground browser accepts one initial URL");
  return { standalone: true, argv };
}
