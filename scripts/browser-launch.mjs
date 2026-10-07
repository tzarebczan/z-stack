/** Test runner override for hosts needing a browser-specific compatibility launcher. */
export function launchBrowser(type, options = {}) {
  const executablePath = process.env[`Z_STACK_${type.name().toUpperCase()}_EXECUTABLE`];
  return type.launch({ ...options, ...(executablePath ? { executablePath } : {}) });
}
