// abc's text link: a React Aria Link in the accent colour, navigating through the router.

import { Link as AriaLink, type LinkProps } from "react-aria-components";

export function Link(props: Omit<LinkProps, "className">) {
  return (
    <AriaLink
      {...props}
      className="text-accent underline outline-none data-hovered:text-accent-strong data-focus-visible:outline-2 data-focus-visible:outline-offset-2 data-focus-visible:outline-accent"
    />
  );
}
