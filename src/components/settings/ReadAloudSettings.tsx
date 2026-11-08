import React, { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { listen } from "@tauri-apps/api/event";
import { commands } from "@/bindings";
import { useModelStore } from "@/stores/modelStore";
import { useSettings } from "../../hooks/useSettings";
import { SettingsGroup } from "../ui/SettingsGroup";
import { SettingContainer } from "../ui/SettingContainer";
import { Slider } from "../ui/Slider";
import { Button } from "../ui/Button";
import { ShortcutInput } from "./ShortcutInput";

/** Download id of the voice (`TTS_MODEL_ID` in Rust). */
const VOICE_ID = "read-aloud-voice";

/** Select text anywhere, press a shortcut, hear it read with Kokoro. */
export const ReadAloudSettings: React.FC = () => {
  const { t } = useTranslation();
  const { getSetting, updateSetting } = useSettings();
  const [downloaded, setDownloaded] = useState<boolean | null>(null);
  const [starting, setStarting] = useState(false);
  const progress = useModelStore((s) => s.downloadProgress[VOICE_ID]);
  const downloading = starting || progress !== undefined;
  const percent = Math.round(progress?.percentage ?? 0);

  const refresh = useCallback(
    () =>
      commands
        .getReadAloudStatus()
        .then((status) => setDownloaded(status.voice_downloaded)),
    [],
  );

  useEffect(() => {
    refresh();
    const unlisten = listen("model-download-complete", refresh);
    return () => {
      unlisten.then((stop) => stop());
    };
  }, [refresh]);

  const download = async () => {
    setStarting(true);
    try {
      // Failures surface through the model store's download-failed listener.
      await commands.downloadReadAloudVoice();
    } finally {
      setStarting(false);
    }
    await refresh();
  };

  const cancel = () => {
    setStarting(false);
    return useModelStore.getState().cancelDownload(VOICE_ID);
  };

  const speed = getSetting("read_aloud_speed") ?? 1;

  return (
    <SettingsGroup title={t("settings.readAloud.title")}>
      <ShortcutInput shortcutId="read_aloud" grouped={true} />
      <SettingContainer
        title={t("settings.readAloud.voice.title")}
        description={t("settings.readAloud.voice.description")}
        descriptionMode="tooltip"
        grouped
      >
        {downloaded ? (
          <span className="text-sm text-ink-3">
            {t("settings.readAloud.voice.ready")}
          </span>
        ) : downloading ? (
          <span className="flex items-center gap-2">
            <span role="status" className="text-sm tabular-nums text-ink-3">
              {t("settings.readAloud.voice.downloading", { percent })}
            </span>
            <Button variant="ghost" size="sm" onClick={cancel}>
              {t("settings.readAloud.voice.cancel")}
            </Button>
          </span>
        ) : (
          <Button
            variant="primary-soft"
            size="sm"
            onClick={download}
            disabled={downloaded === null}
          >
            {t("settings.readAloud.voice.download")}
          </Button>
        )}
      </SettingContainer>
      <Slider
        value={speed}
        onChange={(value: number) => updateSetting("read_aloud_speed", value)}
        min={0.5}
        max={2}
        step={0.05}
        label={t("settings.readAloud.speed.title")}
        description={t("settings.readAloud.speed.description")}
        descriptionMode="tooltip"
        grouped
        formatValue={(value) => `${value.toFixed(2)}×`}
      />
      <SettingContainer
        title={t("settings.readAloud.preview.title")}
        description={t("settings.readAloud.preview.description")}
        descriptionMode="tooltip"
        grouped
        disabled={!downloaded}
      >
        <span className="flex items-center gap-2">
          <Button
            variant="secondary"
            size="sm"
            disabled={!downloaded}
            onClick={() =>
              commands.readAloudText(t("settings.readAloud.preview.sample"))
            }
          >
            {t("settings.readAloud.preview.play")}
          </Button>
          <Button
            variant="ghost"
            size="sm"
            disabled={!downloaded}
            onClick={() => commands.stopReadAloud()}
          >
            {t("settings.readAloud.preview.stop")}
          </Button>
        </span>
      </SettingContainer>
    </SettingsGroup>
  );
};
