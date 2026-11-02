#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DateLanguage {
    American,
    German,
    French,
    Spanish,
    Italian,
    Portuguese,
    Dutch,
}

struct Names {
    language: &'static str,
    months: [&'static str; 12],
    month_abbreviations: [&'static str; 12],
    days: [&'static str; 7],
    day_abbreviations: [&'static str; 7],
}

const AMERICAN: Names = Names {
    language: "AMERICAN",
    months: crate::oracle_datetime::MONTHS,
    month_abbreviations: [
        "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
    ],
    days: crate::oracle_datetime::DAYS,
    day_abbreviations: ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"],
};

const GERMAN: Names = Names {
    language: "GERMAN",
    months: [
        "JANUAR",
        "FEBRUAR",
        "MÄRZ",
        "APRIL",
        "MAI",
        "JUNI",
        "JULI",
        "AUGUST",
        "SEPTEMBER",
        "OKTOBER",
        "NOVEMBER",
        "DEZEMBER",
    ],
    month_abbreviations: [
        "JAN", "FEB", "MRZ", "APR", "MAI", "JUN", "JUL", "AUG", "SEP", "OKT", "NOV", "DEZ",
    ],
    days: [
        "SONNTAG",
        "MONTAG",
        "DIENSTAG",
        "MITTWOCH",
        "DONNERSTAG",
        "FREITAG",
        "SAMSTAG",
    ],
    day_abbreviations: ["SO", "MO", "DI", "MI", "DO", "FR", "SA"],
};

const FRENCH: Names = Names {
    language: "FRENCH",
    months: [
        "JANVIER",
        "FÉVRIER",
        "MARS",
        "AVRIL",
        "MAI",
        "JUIN",
        "JUILLET",
        "AOÛT",
        "SEPTEMBRE",
        "OCTOBRE",
        "NOVEMBRE",
        "DÉCEMBRE",
    ],
    month_abbreviations: [
        "JANV.", "FÉVR.", "MARS", "AVR.", "MAI", "JUIN", "JUIL.", "AOÛT", "SEPT.", "OCT.", "NOV.",
        "DÉC.",
    ],
    days: [
        "DIMANCHE", "LUNDI", "MARDI", "MERCREDI", "JEUDI", "VENDREDI", "SAMEDI",
    ],
    day_abbreviations: ["DIM.", "LUN.", "MAR.", "MER.", "JEU.", "VEN.", "SAM."],
};

const SPANISH: Names = Names {
    language: "SPANISH",
    months: [
        "ENERO",
        "FEBRERO",
        "MARZO",
        "ABRIL",
        "MAYO",
        "JUNIO",
        "JULIO",
        "AGOSTO",
        "SEPTIEMBRE",
        "OCTUBRE",
        "NOVIEMBRE",
        "DICIEMBRE",
    ],
    month_abbreviations: [
        "ENE", "FEB", "MAR", "ABR", "MAY", "JUN", "JUL", "AGO", "SEP", "OCT", "NOV", "DIC",
    ],
    days: [
        "DOMINGO",
        "LUNES",
        "MARTES",
        "MIÉRCOLES",
        "JUEVES",
        "VIERNES",
        "SÁBADO",
    ],
    day_abbreviations: ["DOM", "LUN", "MAR", "MIÉ", "JUE", "VIE", "SÁB"],
};

const ITALIAN: Names = Names {
    language: "ITALIAN",
    months: [
        "GENNAIO",
        "FEBBRAIO",
        "MARZO",
        "APRILE",
        "MAGGIO",
        "GIUGNO",
        "LUGLIO",
        "AGOSTO",
        "SETTEMBRE",
        "OTTOBRE",
        "NOVEMBRE",
        "DICEMBRE",
    ],
    month_abbreviations: [
        "GEN", "FEB", "MAR", "APR", "MAG", "GIU", "LUG", "AGO", "SET", "OTT", "NOV", "DIC",
    ],
    days: [
        "DOMENICA",
        "LUNEDÌ",
        "MARTEDÌ",
        "MERCOLEDÌ",
        "GIOVEDÌ",
        "VENERDÌ",
        "SABATO",
    ],
    day_abbreviations: ["DOM", "LUN", "MAR", "MER", "GIO", "VEN", "SAB"],
};

const PORTUGUESE: Names = Names {
    language: "PORTUGUESE",
    months: [
        "JANEIRO",
        "FEVEREIRO",
        "MARÇO",
        "ABRIL",
        "MAIO",
        "JUNHO",
        "JULHO",
        "AGOSTO",
        "SETEMBRO",
        "OUTUBRO",
        "NOVEMBRO",
        "DEZEMBRO",
    ],
    month_abbreviations: [
        "JAN", "FEV", "MAR", "ABR", "MAI", "JUN", "JUL", "AGO", "SET", "OUT", "NOV", "DEZ",
    ],
    days: [
        "DOMINGO",
        "SEGUNDA-FEIRA",
        "TERÇA-FEIRA",
        "QUARTA-FEIRA",
        "QUINTA-FEIRA",
        "SEXTA-FEIRA",
        "SÁBADO",
    ],
    day_abbreviations: ["DOM", "SEG", "TER", "QUA", "QUI", "SEX", "SÁB"],
};

const DUTCH: Names = Names {
    language: "DUTCH",
    months: [
        "JANUARI",
        "FEBRUARI",
        "MAART",
        "APRIL",
        "MEI",
        "JUNI",
        "JULI",
        "AUGUSTUS",
        "SEPTEMBER",
        "OKTOBER",
        "NOVEMBER",
        "DECEMBER",
    ],
    month_abbreviations: [
        "JAN", "FEB", "MRT", "APR", "MEI", "JUN", "JUL", "AUG", "SEP", "OKT", "NOV", "DEC",
    ],
    days: [
        "ZONDAG",
        "MAANDAG",
        "DINSDAG",
        "WOENSDAG",
        "DONDERDAG",
        "VRIJDAG",
        "ZATERDAG",
    ],
    day_abbreviations: ["ZO", "MA", "DI", "WO", "DO", "VR", "ZA"],
};

const LANGUAGES: [DateLanguage; 7] = [
    DateLanguage::American,
    DateLanguage::German,
    DateLanguage::French,
    DateLanguage::Spanish,
    DateLanguage::Italian,
    DateLanguage::Portuguese,
    DateLanguage::Dutch,
];

fn widest(names: &[&str]) -> usize {
    names
        .iter()
        .map(|name| name.chars().count())
        .max()
        .unwrap_or(0)
}

impl DateLanguage {
    pub(crate) fn named(name: &str) -> Option<Self> {
        let wanted = name.trim();
        LANGUAGES
            .into_iter()
            .find(|language| language.names().language.eq_ignore_ascii_case(wanted))
    }

    fn names(self) -> &'static Names {
        match self {
            Self::American => &AMERICAN,
            Self::German => &GERMAN,
            Self::French => &FRENCH,
            Self::Spanish => &SPANISH,
            Self::Italian => &ITALIAN,
            Self::Portuguese => &PORTUGUESE,
            Self::Dutch => &DUTCH,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        self.names().language
    }

    pub(crate) fn months(self) -> &'static [&'static str; 12] {
        &self.names().months
    }

    pub(crate) fn month_abbreviations(self) -> &'static [&'static str; 12] {
        &self.names().month_abbreviations
    }

    pub(crate) fn days(self) -> &'static [&'static str; 7] {
        &self.names().days
    }

    pub(crate) fn day_abbreviations(self) -> &'static [&'static str; 7] {
        &self.names().day_abbreviations
    }

    pub(crate) fn month_width(self) -> usize {
        widest(self.months())
    }

    pub(crate) fn day_width(self) -> usize {
        widest(self.days())
    }
}

#[cfg(test)]
mod tests {
    use super::{DateLanguage, LANGUAGES};

    #[test]
    fn every_language_is_found_by_its_oracle_name_in_any_case() {
        for language in LANGUAGES {
            assert_eq!(
                DateLanguage::named(&language.name().to_ascii_lowercase()),
                Some(language)
            );
        }
        assert_eq!(DateLanguage::named("KLINGON"), None);
    }

    #[test]
    fn names_are_widest_where_oracle_pads_them() {
        assert_eq!(DateLanguage::American.month_width(), 9);
        assert_eq!(DateLanguage::American.day_width(), 9);
        assert_eq!(DateLanguage::German.day_width(), 10);
        assert_eq!(DateLanguage::Portuguese.day_width(), 13);
        assert_eq!(DateLanguage::French.month_abbreviations()[1], "FÉVR.");
    }
}
