// Реестр переводов: код языка → словарь. en — эталон (en.ts), в реестр не входит.
//
// Новый язык (подробно — TRANSLATING.md):
//   1. файл <код>.ts рядом, формат — как в TRANSLATING.md;
//   2. импорт ниже, по алфавиту кодов: `import de from "./de.ts";`
//   3. строка в LOCALES, тоже по алфавиту: `de,` или `"pt-br": ptBr,`.
// Код — как в списке языков Obsidian (LANGUAGES.md), в нижнем регистре: zh, zh-tw, pt-br.

import type { Dict } from "./en.ts";
import af from "./af.ts";
import am from "./am.ts";
import ar from "./ar.ts";
import az from "./az.ts";
import be from "./be.ts";
import bg from "./bg.ts";
import bn from "./bn.ts";
import ca from "./ca.ts";
import cs from "./cs.ts";
import da from "./da.ts";
import de from "./de.ts";
import dv from "./dv.ts";
import el from "./el.ts";
import eo from "./eo.ts";
import es from "./es.ts";
import eu from "./eu.ts";
import fa from "./fa.ts";
import fi from "./fi.ts";
import fr from "./fr.ts";
import ga from "./ga.ts";
import gl from "./gl.ts";
import he from "./he.ts";
import hi from "./hi.ts";
import hr from "./hr.ts";
import hu from "./hu.ts";
import id from "./id.ts";
import it from "./it.ts";
import ja from "./ja.ts";
import ka from "./ka.ts";
import kab from "./kab.ts";
import km from "./km.ts";
import kn from "./kn.ts";
import ko from "./ko.ts";
import ky from "./ky.ts";
import la from "./la.ts";
import lt from "./lt.ts";
import lv from "./lv.ts";
import ml from "./ml.ts";
import ms from "./ms.ts";
import nanTw from "./nan-tw.ts";
import ne from "./ne.ts";
import nl from "./nl.ts";
import nn from "./nn.ts";
import no from "./no.ts";
import oc from "./oc.ts";
import or from "./or.ts";
import pl from "./pl.ts";
import pt from "./pt.ts";
import ptBr from "./pt-br.ts";
import ro from "./ro.ts";
import ru from "./ru.ts";
import sa from "./sa.ts";
import si from "./si.ts";
import sk from "./sk.ts";
import sl from "./sl.ts";
import sq from "./sq.ts";
import sr from "./sr.ts";
import sv from "./sv.ts";
import sw from "./sw.ts";
import ta from "./ta.ts";
import te from "./te.ts";
import th from "./th.ts";
import tl from "./tl.ts";
import tr from "./tr.ts";
import tt from "./tt.ts";
import uk from "./uk.ts";
import ur from "./ur.ts";
import uz from "./uz.ts";
import vi from "./vi.ts";
import zh from "./zh.ts";
import zhTw from "./zh-tw.ts";

export const LOCALES: Record<string, Dict> = {
  af,
  am,
  ar,
  az,
  be,
  bg,
  bn,
  ca,
  cs,
  da,
  de,
  dv,
  el,
  eo,
  es,
  eu,
  fa,
  fi,
  fr,
  ga,
  gl,
  he,
  hi,
  hr,
  hu,
  id,
  it,
  ja,
  ka,
  kab,
  km,
  kn,
  ko,
  ky,
  la,
  lt,
  lv,
  ml,
  ms,
  "nan-tw": nanTw,
  ne,
  nl,
  nn,
  no,
  oc,
  or,
  pl,
  pt,
  "pt-br": ptBr,
  ro,
  ru,
  sa,
  si,
  sk,
  sl,
  sq,
  sr,
  sv,
  sw,
  ta,
  te,
  th,
  tl,
  tr,
  tt,
  uk,
  ur,
  uz,
  vi,
  zh,
  "zh-tw": zhTw,
};
