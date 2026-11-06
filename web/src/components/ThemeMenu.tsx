// The theme choice in the shell: a menu of Light, Dark and System (follow the operating system),
// from shadcn's DropdownMenu. Its button names the choice in force.

import { MonitorIcon, MoonIcon, SunIcon } from "lucide-react";

import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuLabel,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  chooseTheme,
  isThemeChoice,
  THEME_CHOICES,
  type ThemeChoice,
  useThemeChoice,
} from "@/theme";

const LABELS: Record<ThemeChoice, string> = { light: "Light", dark: "Dark", system: "System" };

function ThemeIcon({ choice }: { readonly choice: ThemeChoice }) {
  switch (choice) {
    case "light":
      return <SunIcon aria-hidden="true" />;
    case "dark":
      return <MoonIcon aria-hidden="true" />;
    case "system":
      return <MonitorIcon aria-hidden="true" />;
  }
}

export function ThemeMenu() {
  const choice = useThemeChoice();
  return (
    <DropdownMenu>
      <DropdownMenuTrigger
        render={
          <Button variant="ghost" size="icon" aria-label={`Theme: ${LABELS[choice]}`}>
            <ThemeIcon choice={choice} />
          </Button>
        }
      />
      <DropdownMenuContent align="end" className="w-40">
        <DropdownMenuGroup>
          <DropdownMenuLabel>Theme</DropdownMenuLabel>
          <DropdownMenuRadioGroup
            value={choice}
            onValueChange={(value: unknown) => {
              if (isThemeChoice(value)) {
                chooseTheme(value);
              }
            }}
          >
            {THEME_CHOICES.map((option) => (
              <DropdownMenuRadioItem key={option} value={option} closeOnClick>
                <ThemeIcon choice={option} />
                {LABELS[option]}
              </DropdownMenuRadioItem>
            ))}
          </DropdownMenuRadioGroup>
        </DropdownMenuGroup>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
